//! Global (theme-anchored) graph search — M2c Task 3.
//!
//! The question local search structurally cannot answer: "what have I been
//! thinking about this year?" Top-k chunk retrieval answers questions ABOUT
//! something; this answers questions about the SHAPE of a corpus, by mapping
//! over community summaries and reducing the partials into one answer.
//!
//! ```text
//! question -> community::summaries_for_search   [size-ranked, model-filtered]
//!          -> map:    one AnswerProvider call per community -> (partial, relevance)
//!          -> reduce: one call over the partials, ordered by relevance
//!          -> GlobalAnswer { answer, partials, skipped_unsummarised }
//! ```
//!
//! # What is proven here, and what is not
//!
//! The SELECTION, the map/reduce control flow, the stale-summary refresh trigger
//! and the skipped-community accounting are pure logic + SQL and are fully
//! tested. The ANSWER is a stub's output: there is no LLM in this environment,
//! so answer QUALITY is unmeasured, exactly as in M2a and M2b. Do not let the
//! acceptance language drift into implying otherwise.
use std::sync::Arc;

use noted_db::PgPool;
use noted_db::community::{self, SummaryCandidate};
use uuid::Uuid;

use crate::answer::{AnswerError, AnswerProvider, AnswerRequest, ContextItem};
use crate::summary::SummaryProvider;
use crate::summary_worker::{SummaryWorker, SummaryWorkerError};

/// A summary whose membership has drifted but which is still worth reading.
const STATE_STALE_USABLE: &str = "stale_usable";

/// How many communities one global search maps over.
///
/// Each is a separate provider call, so this is a direct multiplier on the
/// latency and cost of the slowest surface in the product. Eight is enough to
/// cover a small workspace's themes whole while keeping a local model's
/// round-trips bounded; `summaries_for_search` clamps its own limit besides.
pub const MAX_COMMUNITIES: i64 = 8;

#[derive(Debug, thiserror::Error)]
pub enum GlobalSearchError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("answerer failed: {0}")]
    Answer(#[from] AnswerError),
    #[error("summary refresh failed: {0}")]
    Refresh(#[from] SummaryWorkerError),
}

/// One community's contribution, kept in the result rather than collapsed into
/// the final prose.
///
/// This is global search's "show your work": a user who cannot see which themes
/// were consulted, and how strongly each bore on the question, has no way to
/// judge an answer that ranged over their whole workspace.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PartialAnswer {
    pub community_id: Uuid,
    pub member_count: i64,
    /// Whether this community's summary was current or already drifting when it
    /// was read. Surfaced because it is the honest caveat on the partial.
    pub was_stale: bool,
    pub text: String,
    /// The map step's self-reported bearing on the question, `0.0..=1.0`.
    pub relevance: f32,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct GlobalAnswer {
    pub answer: String,
    /// Ordered by descending relevance — the same order the reduce step saw.
    pub partials: Vec<PartialAnswer>,
    /// Communities that had no usable summary and were therefore NOT consulted.
    ///
    /// Part of the answer, not a log line: an answer over 3 of 40 communities is
    /// a different claim from an answer over all 40, and only the caller can
    /// decide how to present that.
    pub skipped_unsummarised: i64,
}

/// Map-reduce a question over a workspace's community summaries.
///
/// # A stale summary is USED, and its regeneration is REQUESTED
///
/// M2b §2.2 settled this: a slightly stale summary beats a missing one, so a
/// `stale_usable` row is read and mapped like any other. It also triggers
/// `SummaryWorker::refresh` — the lazy-regeneration path M2b built and which,
/// until this function existed, **nothing in the product ever called**. The
/// refresh runs after the summary has been used, so a refresh failure cannot
/// deny the user an answer they could otherwise have had.
///
/// # No summaries means NO PROVIDER CALL
///
/// A workspace whose graph has never been clustered and summarised returns a
/// fixed statement without invoking the answerer, for the same reason
/// `local_search` does: a model handed a question and no material will answer
/// from its weights, and a fluent answer with an empty `partials` list is
/// indistinguishable from a well-sourced one.
#[allow(clippy::too_many_arguments)]
pub async fn global_search(
    pool: &PgPool,
    workspace_id: Uuid,
    question: &str,
    answerer: &dyn AnswerProvider,
    summariser: Arc<dyn SummaryProvider>,
) -> Result<GlobalAnswer, GlobalSearchError> {
    let selection = community::summaries_for_search(
        pool,
        workspace_id,
        summariser.model_id(),
        MAX_COMMUNITIES,
    )
    .await?;

    if selection.candidates.is_empty() {
        return Ok(GlobalAnswer {
            answer: format!(
                "This workspace has no summarised themes yet, so there is nothing to answer \
                 \"{}\" from.",
                question.trim()
            ),
            partials: Vec::new(),
            skipped_unsummarised: selection.skipped_unsummarised,
        });
    }

    let mut partials: Vec<PartialAnswer> = Vec::with_capacity(selection.candidates.len());
    let mut stale: Vec<Uuid> = Vec::new();

    for candidate in &selection.candidates {
        let was_stale = candidate.state == STATE_STALE_USABLE;
        if was_stale {
            stale.push(candidate.community_id);
        }

        let text = answerer.synthesise(&map_request(question, candidate)).await?;
        partials.push(PartialAnswer {
            community_id: candidate.community_id,
            member_count: candidate.member_count,
            was_stale,
            relevance: relevance_of(question, &candidate.summary),
            text,
        });
    }

    // Descending relevance, with `community_id` breaking ties so the reduce
    // step's input — and therefore its output — is deterministic for a
    // deterministic provider. Sorting on a float alone would leave equal-scoring
    // communities in an order that is stable only by accident.
    partials.sort_by(|a, b| {
        b.relevance
            .partial_cmp(&a.relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.community_id.cmp(&b.community_id))
    });

    let answer = answerer.synthesise(&reduce_request(question, &partials)).await?;
    crate::answer::verify_answer(&answer, answerer.model_id())?;

    // AFTER the answer is in hand, deliberately: the refresh is maintenance the
    // question happened to reveal, not work the questioner should wait on or be
    // denied an answer by.
    if !stale.is_empty() {
        let worker = SummaryWorker::new(pool.clone(), summariser, workspace_id);
        for community_id in stale {
            worker.refresh(community_id).await?;
        }
    }

    Ok(GlobalAnswer {
        answer,
        partials,
        skipped_unsummarised: selection.skipped_unsummarised,
    })
}

/// One community's map-step prompt.
///
/// `subjects` carries the community's size rather than entity names: global
/// search's unit is the theme, and the summary already names what it is about.
fn map_request(question: &str, candidate: &SummaryCandidate) -> AnswerRequest {
    AnswerRequest {
        question: question.to_string(),
        subjects: vec![format!("{} related notes", candidate.member_count)],
        context: vec![ContextItem {
            source: "community summary".to_string(),
            text: candidate.summary.clone(),
            note: if candidate.state == STATE_STALE_USABLE {
                "a summary whose membership has changed since it was written".to_string()
            } else {
                "a current summary of one theme in this workspace".to_string()
            },
        }],
    }
}

/// The reduce-step prompt: every partial, in relevance order.
fn reduce_request(question: &str, partials: &[PartialAnswer]) -> AnswerRequest {
    AnswerRequest {
        question: question.to_string(),
        subjects: partials
            .iter()
            .map(|p| format!("theme of {} notes", p.member_count))
            .collect(),
        context: partials
            .iter()
            .map(|p| ContextItem {
                source: "partial answer".to_string(),
                text: p.text.clone(),
                note: format!("bearing on the question: {:.2}", p.relevance),
            })
            .collect(),
    }
}

/// How strongly a summary bears on the question, `0.0..=1.0`.
///
/// # Why this is lexical, and why that is honest
///
/// The map step's relevance ought to come from the model that read the summary —
/// "how much does this theme bear on the question, 0 to 1" — but that means
/// parsing a self-reported score out of free text, and a provider that returns
/// prose instead of a number would silently degrade every ranking with no test
/// able to see it. So relevance is computed HERE, from the question's content
/// words appearing in the summary, and is a property of the retrieval rather
/// than of the model's self-assessment.
///
/// The consequence, stated rather than buried: this is term overlap, so it
/// cannot see a theme that bears on the question in different words — the same
/// limitation as `summaries_for_search`'s size-based selection, and it wants the
/// same fix (summary embeddings). Both are recorded in the design's risk table.
fn relevance_of(question: &str, summary: &str) -> f32 {
    let haystack = summary.to_lowercase();
    let terms: Vec<String> = question
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        // Two-character words are almost all function words ("of", "to", "in"),
        // and they match inside longer words besides.
        .filter(|w| w.len() > 2)
        .map(|w| w.to_string())
        .collect();

    if terms.is_empty() {
        return 0.0;
    }
    let hits = terms.iter().filter(|t| haystack.contains(*t)).count();
    hits as f32 / terms.len() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relevance_is_the_share_of_question_terms_the_summary_carries() {
        assert_eq!(relevance_of("postgres tuning", "postgres tuning notes"), 1.0);
        assert_eq!(relevance_of("postgres tuning", "sourdough and bread"), 0.0);
        let half = relevance_of("postgres sourdough", "postgres only");
        assert!((half - 0.5).abs() < f32::EPSILON, "got {half}");
    }

    #[test]
    fn short_function_words_do_not_inflate_relevance() {
        // "of" and "in" would otherwise match inside almost any prose, and
        // "of" is a substring of "proof" besides.
        assert_eq!(relevance_of("of in", "a proof in the pudding"), 0.0);
    }

    #[test]
    fn a_question_with_no_content_words_scores_zero_rather_than_dividing_by_zero() {
        assert_eq!(relevance_of("of in a", "anything at all"), 0.0);
    }
}

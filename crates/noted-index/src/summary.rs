//! `SummaryProvider` — the community summariser, and a deterministic stub.
//!
//! The FOURTH use of this project's provider pattern (`EmbeddingProvider`
//! M1b, `ExtractionProvider` M2a, `extract_providers::OllamaExtractor` M2a).
//! Same shape for the same reason: **there is still no LLM in this
//! environment**, so every property M2b proves is proven against a
//! deterministic stub and the real-model run stays a documented operator step.
//!
//! # No real provider ships here, deliberately
//!
//! M2a shipped its Ollama-backed `ExtractionProvider` as a SEPARATE task, not
//! alongside the trait, and this milestone's plan lists only the trait and the
//! stub for Task 4. A real `OllamaSummariser` would need its own module plus a
//! `summary-ollama` feature (an HTTP client for a service that does not exist
//! in this environment), and it would carry the same non-negotiable that
//! `extract_providers` documents at length: **an explicit `reqwest` request
//! timeout AND connect timeout.** reqwest applies neither by default, and a
//! summariser that hangs is worse than one that fails, because a call that
//! never returns is never counted by any failure cap. That is recorded here so
//! whoever writes the real provider does not have to rediscover it.

use uuid::Uuid;

/// One member of a community, as the summariser sees it.
///
/// `name` is the entity's canonical per-workspace name (`UNIQUE (workspace_id,
/// name)`), which is also the stable natural key the clusterer orders by.
/// `entity_type` and `description` are whatever the extraction provider
/// classified it as — carried through because they are exactly the context a
/// real model needs to write prose about a cluster, and because a stub that
/// only ever saw names could not vary its output along them.
///
/// `entity_type` is `String`, not `Option<String>`: `entities.entity_type` is
/// `text NOT NULL` (migration `0005_graph.sql`). Modelling it as optional would
/// add a provider branch nothing can reach — the exact trap Task 2 and Task 3
/// each found once (a documented mechanism no mutation could kill).
/// `description` IS nullable there, and `resolve_entity` leaves it `None`
/// whenever the extractor supplied none, so that one is genuinely optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunityMember {
    pub id: Uuid,
    pub name: String,
    pub entity_type: String,
    pub description: Option<String>,
}

/// Everything a summariser is given about one community.
///
/// `members` arrive **ordered by `name` ascending**, canonically, and callers
/// must keep it that way — see `summary_worker::community_facts`. A
/// deterministic provider that received the same member set in two different
/// orders would otherwise produce two different summaries for a community that
/// did not change, which defeats the entire point of `member_set_hash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunityFacts {
    pub community_id: Uuid,
    pub level: i32,
    pub members: Vec<CommunityMember>,
}

#[derive(Debug, thiserror::Error)]
pub enum SummaryError {
    #[error("summariser model error: {0}")]
    Model(String),
    #[error("summariser output invalid: {0}")]
    Invalid(String),
}

/// Reject a summary that is empty or nothing but whitespace.
///
/// A real model that runs out of context, refuses, or emits only a stripped
/// markdown fence returns "" rather than failing, and `community_summaries.summary`
/// is `text NOT NULL` with no CHECK — so nothing downstream stops an empty
/// string being stored as though it were a summary. It would then satisfy the
/// set-difference queue (the row exists, the hash matches) and the community
/// would be permanently "summarised" with nothing in it, silently, forever.
///
/// Rejecting instead surfaces as `SummaryError::Invalid`, which
/// `SummaryWorker::run_once` treats exactly as it treats a model failure: that
/// one community is skipped and STAYS IN THE QUEUE, so the next pass retries
/// it. Same error-direction reasoning as M2a's 0008 backfill — under-marking
/// costs a redundant recompute and self-corrects, over-marking is permanent and
/// silent.
///
/// `pub` so a real provider can reuse it (the same reason
/// `provider::verify_batch` is) and so the integration tests, which compile as
/// a separate crate, can exercise it directly.
pub fn verify_summary(summary: &str, model: &str) -> Result<(), SummaryError> {
    if summary.trim().is_empty() {
        return Err(SummaryError::Invalid(format!(
            "summariser '{model}' returned an empty summary; refusing to store it"
        )));
    }
    Ok(())
}

#[async_trait::async_trait]
pub trait SummaryProvider: Send + Sync {
    /// Recorded in `community_summaries.model_id`, and compared against it to
    /// decide whether a stored summary was written by THIS summariser. Must
    /// come from here and never from a literal at the call site (M1c lesson).
    fn model_id(&self) -> &str;

    async fn summarise(&self, facts: &CommunityFacts) -> Result<String, SummaryError>;
}

/// Deterministic, no-model summariser for tests.
///
/// # It VARIES, and that is a requirement rather than decoration
///
/// M2a's `StubExtractor` was so uniform — single-token names, one entity type,
/// one relation, a constant weight — that four production paths turned out to
/// be structurally unreachable by any test using it, and had to be covered
/// retroactively with hand-built fixtures. The lesson, recorded in
/// `.superpowers/sdd/progress.md`, is that a stub's job is not only to be
/// deterministic but to be DISCRIMINATING: a constant output makes whole
/// classes of wiring bug invisible.
///
/// So this one's output is a function of the actual membership, and varies
/// along every axis the worker cares about:
///
///   * **Which community.** Two different member sets produce two different
///     strings, so "the right summary landed on the right community" is an
///     assertable property. Under a constant stub, a worker that wrote every
///     summary to the same community, or paired community *i*'s prose with
///     community *j*'s row, would pass every test.
///   * **Member ORDER.** The text lists members in the order given, so a facts
///     query that stopped ordering by `name` produces a different summary for
///     an unchanged community — which is exactly the bug that would make a
///     deterministic pipeline look non-deterministic.
///   * **Size, in prose SHAPE and not just length.** A one-member community
///     reads "X stands alone."; larger ones get a "with N members" clause. Two
///     structurally different outputs, so a size-dependent code path has
///     something to distinguish.
///   * **Entity type and description**, when present, so the fields are not
///     silently dropped somewhere between the database and the provider.
///
/// It never fails and never returns empty. The failure paths
/// (`SummaryError::Model`, `verify_summary`'s empty-output rejection) are
/// reached by purpose-built providers in `tests/summary.rs` instead — a stub
/// that failed on a magic member name would be a trap of its own.
pub struct StubSummariser;

impl StubSummariser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubSummariser {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SummaryProvider for StubSummariser {
    fn model_id(&self) -> &str {
        "stub-summariser-v1"
    }

    async fn summarise(&self, facts: &CommunityFacts) -> Result<String, SummaryError> {
        let described: Vec<String> = facts
            .members
            .iter()
            .map(|m| match &m.description {
                Some(d) => format!("{} ({}: {d})", m.name, m.entity_type),
                None => format!("{} ({})", m.name, m.entity_type),
            })
            .collect();

        Ok(match described.len() {
            0 => format!("Level {} community with no members.", facts.level),
            1 => format!("{} stands alone at level {}.", described[0], facts.level),
            n => format!(
                "A level {} community with {n} members: {}.",
                facts.level,
                described.join(", ")
            ),
        })
    }
}

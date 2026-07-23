//! `AnswerProvider` — synthesis over retrieved context, and a deterministic stub.
//!
//! The FIFTH use of this project's provider pattern (`EmbeddingProvider` M1b,
//! `ExtractionProvider` M2a, `extract_providers::OllamaExtractor` M2a,
//! `SummaryProvider` M2b). Same shape for the same reason: **there is still no
//! LLM in this environment**, so spec §2.2 splits M2c explicitly — retrieval is
//! pure SQL and is fully tested, synthesis is stubbed and the real-model run
//! stays a documented operator step. Acceptance language must keep saying which
//! half is proven.
//!
//! # No real provider ships here, deliberately — and the non-negotiable if one does
//!
//! M2a shipped its Ollama-backed `ExtractionProvider` as a separate, gated
//! module, and M2b's `summary.rs` records why it did the same. This module
//! follows both. Whoever writes `answer_providers::OllamaAnswerer` MUST set an
//! explicit `reqwest` **request timeout AND connect timeout**. reqwest applies
//! NEITHER by default: `Client::new()` waits forever. M2a shipped exactly that
//! and the consequence is not "a slow answer" — it is that a hung model stalls
//! the caller indefinitely and **no failure cap can ever trip, because a call
//! that never returns is never counted**. A synthesiser that fails is strictly
//! better than one that hangs. Recorded here so it is not rediscovered the hard
//! way a third time.
//!
//! # What this trait deliberately does NOT carry yet
//!
//! Spec §4's global search wants a self-reported RELEVANCE score back from each
//! per-community map call. That is not modelled here, because local search makes
//! exactly one call and so has nothing to order — a `relevance` field would be a
//! documented mechanism no test on this task could kill, which is precisely the
//! defect class this project has now found in four consecutive tasks. Task 3
//! adds it alongside the test that needs it.

/// One piece of retrieved context put in front of the synthesiser.
///
/// Deliberately prose-only: no uuids, no content hashes. A model does not need
/// them, and the citation record that DOES carry them
/// (`graph_search::Citation`) is built from the retrieval rows directly, so the
/// provenance a user sees can never be something a model chose to echo back.
///
/// `note` is the "why is this in front of you" line — local search fills it from
/// `GraphHit::hops` via `hop_note`. It is part of the prompt on purpose: an
/// answer that leans on a 2-hop chunk should be able to say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextItem {
    /// Human-readable origin: a page title for local search, a community label
    /// for global search.
    pub source: String,
    /// The passage itself — a chunk snippet, or a community summary.
    pub text: String,
    /// Why this passage was retrieved. See `hop_note`.
    pub note: String,
}

/// Everything a synthesiser is given for one call.
///
/// `context` arrives in RANKED order (best first) and callers must keep it that
/// way: a provider that truncates to fit a context window truncates the tail, so
/// the order is load-bearing rather than cosmetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerRequest {
    pub question: String,
    /// The named things retrieval decided the question is ABOUT — local search's
    /// seed entities, by name, in the order `graph_search::seed_entities`
    /// returns them. Legitimately empty (a workspace with no graph still
    /// retrieves chunks), and the stub varies its shape on that.
    pub subjects: Vec<String>,
    pub context: Vec<ContextItem>,
}

/// The `note` a local-search citation and prompt line carry for a hit `hops`
/// steps from a seed entity.
///
/// `pub` because both `graph_search`'s prompt construction and its tests need
/// the same string, and a second copy of it would drift.
pub fn hop_note(hops: i32) -> String {
    match hops {
        0 => "found directly by search".to_string(),
        1 => "1 hop from a subject of the question".to_string(),
        n => format!("{n} hops from a subject of the question"),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AnswerError {
    #[error("answer model error: {0}")]
    Model(String),
    #[error("answer output invalid: {0}")]
    Invalid(String),
}

/// Reject an answer that is empty or nothing but whitespace.
///
/// Exactly `summary::verify_summary`'s reasoning, and for exactly the same
/// failure: a real model that runs out of context, refuses, or emits only a
/// markdown fence that gets stripped returns `""` rather than failing. An empty
/// string presented to a user underneath a list of citations reads as "the
/// system found evidence and had nothing to say about it", which is worse than
/// an error — it is an error wearing a successful response's clothes.
///
/// `pub` so a real provider can reuse it (the same reason `verify_summary` and
/// `provider::verify_batch` are) and so the integration tests, which compile as
/// a separate crate, can exercise it directly.
pub fn verify_answer(answer: &str, model: &str) -> Result<(), AnswerError> {
    if answer.trim().is_empty() {
        return Err(AnswerError::Invalid(format!(
            "answerer '{model}' returned an empty answer; refusing to present it"
        )));
    }
    Ok(())
}

/// The prompt handed to any answer provider, built from retrieval rows.
///
/// Lives here rather than on a provider because it is model-agnostic: it
/// describes the retrieval contract (passages, their provenance note, the
/// subjects local search seeded from), not any one backend's wire format. Two
/// copies would drift, and a drifted prompt is invisible — both providers keep
/// returning plausible prose while one of them silently stops being told which
/// passages were graph hops.
pub fn build_answer_prompt(req: &AnswerRequest) -> String {
    let mut p = String::new();
    p.push_str(
        "Answer the question using ONLY the passages below. \
         If they do not contain the answer, say so plainly rather than guessing. \
         Do not invent sources. \
         Reply in complete sentences that would make sense to someone who has \
         not seen the passages — not a bare name or fragment.\n\n",
    );
    if !req.subjects.is_empty() {
        p.push_str(&format!("The question is about: {}\n\n", req.subjects.join(", ")));
    }
    for (i, item) in req.context.iter().enumerate() {
        p.push_str(&format!(
            "[{}] from \"{}\" ({}):\n{}\n\n",
            i + 1,
            item.source,
            item.note,
            item.text
        ));
    }
    p.push_str(&format!("Question: {}\nAnswer:", req.question));
    p
}

#[async_trait::async_trait]
pub trait AnswerProvider: Send + Sync {
    /// Identifies the synthesiser. Must come from here and never from a literal
    /// at the call site — the M1c lesson every other provider in this crate
    /// carries.
    fn model_id(&self) -> &str;

    async fn synthesise(&self, req: &AnswerRequest) -> Result<String, AnswerError>;
}

/// Deterministic, no-model answerer for tests.
///
/// # It VARIES, and that is a requirement rather than decoration
///
/// M2a's `StubExtractor` was so uniform — single-token names, one entity type,
/// one relation, a constant weight — that FOUR production paths turned out to be
/// structurally unreachable by any test using it and had to be covered
/// retroactively with hand-built fixtures. A stub's job is not only to be
/// deterministic but to be DISCRIMINATING: under a constant stub, every wiring
/// bug between the retrieval and the prompt is invisible.
///
/// So every field of `AnswerRequest` reaches the output, and the output's SHAPE
/// (not merely its length) changes along the axes local search actually varies:
///
///   * **Empty vs non-empty context.** A distinct sentence, so "answered from
///     nothing" can never be mistaken for "answered from something". Local
///     search never reaches it — it declines to call a provider it has no
///     evidence for (`graph_search::local_search`) — so it is pinned by a direct
///     unit test instead of being left as an unreachable branch.
///   * **How many passages**, in prose shape: one passage reads "one passage";
///     several get a count. A count that came from the wrong list has somewhere
///     to show up.
///   * **The seed-vs-hop MIX**, because each item's `note` is echoed verbatim.
///     If local search ever passed a constant `hops`, or lost the field between
///     the SQL row and the prompt, this text changes. That is the whole reason
///     `GraphHit::hops` exists.
///   * **Which subjects**, in order — so `seed_entities` reaching the prompt is
///     observable, and so is a caller that stopped ordering them by name.
///   * **The question**, echoed, so a call made with the wrong string is not a
///     silent no-op.
///
/// It never fails and never returns empty. The failure paths (`AnswerError::Model`,
/// `verify_answer`'s empty-output rejection) get purpose-built providers in
/// `tests/local_search.rs` — a stub that failed on a magic question would be a
/// trap of its own, which is the note `StubSummariser` already carries.
pub struct StubAnswerer;

impl StubAnswerer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubAnswerer {
    fn default() -> Self {
        Self::new()
    }
}

/// First `n` whitespace-separated words, so one long chunk cannot drown the rest
/// of the stub's output while still letting two different chunks be told apart.
fn opening(text: &str, n: usize) -> String {
    text.split_whitespace().take(n).collect::<Vec<_>>().join(" ")
}

#[async_trait::async_trait]
impl AnswerProvider for StubAnswerer {
    fn model_id(&self) -> &str {
        "stub-answerer-v1"
    }

    async fn synthesise(&self, req: &AnswerRequest) -> Result<String, AnswerError> {
        let q = req.question.trim();

        if req.context.is_empty() {
            return Ok(format!("Nothing on file bears on \"{q}\"."));
        }

        let subjects = if req.subjects.is_empty() {
            "no named subject".to_string()
        } else {
            req.subjects.join(", ")
        };

        let cited: Vec<String> = req
            .context
            .iter()
            .map(|c| format!("{} [{}] \"{}\"", c.source, c.note, opening(&c.text, 8)))
            .collect();

        Ok(match cited.len() {
            1 => format!("On \"{q}\" (about {subjects}), one passage: {}.", cited[0]),
            n => format!(
                "On \"{q}\" (about {subjects}), {n} passages: {}.",
                cited.join("; ")
            ),
        })
    }
}

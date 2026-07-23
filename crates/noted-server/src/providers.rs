//! Resolving the three model roles from configuration.
//!
//! Extraction, answer synthesis and community summarisation are configured
//! INDEPENDENTLY, because they have genuinely different shapes. Extraction runs
//! once per chunk across a whole corpus — throughput-bound, mechanical, and the
//! place a per-token bill actually accumulates. Synthesis runs once per question
//! with a human waiting — latency-bound, and the one role where reasoning is the
//! product. An operator can reasonably want a local model grinding through
//! extraction overnight and a hosted one answering questions, so the
//! configuration lets them say that instead of forcing one backend on all three.
//!
//! # The spec grammar
//!
//! ```text
//! NOTED_EXTRACT=stub | ollama:<model> | gemini:<model>
//! NOTED_ANSWER=stub  | ollama:<model> | gemini:<model>
//! NOTED_SUMMARY=stub | ollama:<model> | gemini:<model>
//! ```
//!
//! With `GEMINI_API_KEY` for the hosted backend and `NOTED_OLLAMA_URL`
//! (default `http://localhost:11434`) for the local one.
//!
//! # Why a misconfiguration is fatal here
//!
//! Every function below returns `Err` rather than falling back to a stub. An
//! operator who sets `NOTED_ANSWER=gemini:...` and mistypes the key has said
//! clearly what they want; quietly serving stub prose instead would look like a
//! working deployment and produce answers nobody could tell were fake. The
//! failure belongs at startup, where it is one line in a log, rather than in
//! every answer thereafter.
use std::sync::Arc;

use noted_index::answer::{AnswerProvider, StubAnswerer};
use noted_index::extract::{ExtractionProvider, StubExtractor};
use noted_index::summary::{StubSummariser, SummaryProvider};

/// A parsed provider spec. `None` for extraction means "do not extract at all",
/// which is different from "extract with a stub".
enum Spec<'a> {
    Stub,
    Ollama(&'a str),
    Gemini(&'a str),
}

fn parse(spec: &str) -> Result<Spec<'_>, String> {
    let spec = spec.trim();
    if spec == "stub" {
        return Ok(Spec::Stub);
    }
    match spec.split_once(':') {
        Some(("ollama", m)) if !m.is_empty() => Ok(Spec::Ollama(m)),
        Some(("gemini", m)) if !m.is_empty() => Ok(Spec::Gemini(m)),
        _ => Err(format!(
            "unrecognised provider spec {spec:?}; expected `stub`, `ollama:<model>` \
             or `gemini:<model>`"
        )),
    }
}

fn ollama_url() -> String {
    std::env::var("NOTED_OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".to_string())
}

/// Reads the Gemini key at the moment a Gemini provider is actually requested,
/// so a deployment using only Ollama never needs one set.
fn gemini_key() -> Result<String, String> {
    noted_index::gemini::api_key_from_env().map_err(|e| {
        format!("{e}; a `gemini:` provider was configured, so the key is required")
    })
}

/// Reads a role's spec, treating an EMPTY value as unset.
///
/// This matters because `docker-compose.yml` now always sets these three
/// variables (`${NOTED_ANSWER:-stub}` and friends), so a user who writes
/// `NOTED_EXTRACT=` in their `.env` to mean "off" hands the process an empty
/// string rather than nothing at all. Without this, that empty string reaches
/// `parse`, fails, and — because a malformed spec is deliberately fatal — takes
/// the whole server down at startup. "Blank means unset" is what anyone editing
/// a `.env` expects.
fn spec_for(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

/// Builds the extractor. `Ok(None)` means extraction is switched off — the
/// indexing status then reports 0-of-0 rather than a backlog that will never
/// drain.
pub fn extractor() -> Result<Option<Arc<dyn ExtractionProvider>>, String> {
    let Some(spec) = spec_for("NOTED_EXTRACT") else {
        tracing::info!(
            "no extraction provider configured (NOTED_EXTRACT unset); the background \
             indexer will embed but not extract. Set NOTED_EXTRACT=gemini:<model> to \
             build a real knowledge graph."
        );
        return Ok(None);
    };

    Ok(Some(match parse(&spec)? {
        Spec::Stub => {
            tracing::warn!(
                "NOTED_EXTRACT=stub: the knowledge graph will be built by the DETERMINISTIC \
                 STUB extractor, not a real model. Fine for exercising the pipeline; the \
                 resulting graph is not meaningful."
            );
            Arc::new(StubExtractor::new()) as Arc<dyn ExtractionProvider>
        }
        Spec::Ollama(model) => {
            tracing::info!("extraction: ollama model {model} at {}", ollama_url());
            Arc::new(
                noted_index::extract_providers::OllamaExtractor::new(ollama_url(), model),
            )
        }
        Spec::Gemini(model) => {
            tracing::info!("extraction: gemini model {model}");
            Arc::new(
                noted_index::gemini::GeminiExtractor::new(&gemini_key()?, model)
                    .map_err(|e| format!("could not build the Gemini extractor: {e}"))?,
            )
        }
    }))
}

/// Builds the answerer. Defaults to the stub, and says so loudly: an Ask
/// surface returning deterministic prose over real retrieval is the honest
/// default for a deployment with no model, but nobody should discover that from
/// the answers.
pub fn answerer() -> Result<Arc<dyn AnswerProvider>, String> {
    let Some(spec) = spec_for("NOTED_ANSWER") else {
        tracing::warn!(
            "NOTED_ANSWER unset: the Ask surface will return STUB prose over real \
             retrieval. Set NOTED_ANSWER=gemini:<model> for real answers."
        );
        return Ok(Arc::new(StubAnswerer::new()));
    };

    Ok(match parse(&spec)? {
        Spec::Stub => Arc::new(StubAnswerer::new()),
        Spec::Ollama(model) => {
            tracing::info!("answers: ollama model {model} at {}", ollama_url());
            Arc::new(
                noted_index::ollama::OllamaAnswerer::new(&ollama_url(), model)
                    .map_err(|e| format!("could not build the Ollama answerer: {e}"))?,
            )
        }
        Spec::Gemini(model) => {
            tracing::info!("answers: gemini model {model}");
            Arc::new(
                noted_index::gemini::GeminiAnswerer::new(&gemini_key()?, model)
                    .map_err(|e| format!("could not build the Gemini answerer: {e}"))?,
            )
        }
    })
}

/// Builds the summariser used by global search and by the background
/// summary pass.
///
/// Returns `None` when no REAL model is configured, and that distinction
/// matters more here than for the answerer. A stub answer is transient — it is
/// produced per request and never stored. A community summary is PERSISTED, so
/// a stub one fills `community_summaries` with deterministic placeholder prose
/// that global search then answers from, and a reader has no way to tell it
/// from a real summary. The caller substitutes a stub for the read path if it
/// wants one; the background pass must not run at all.
pub fn summariser() -> Result<Option<Arc<dyn SummaryProvider>>, String> {
    let Some(spec) = spec_for("NOTED_SUMMARY") else {
        return Ok(None);
    };

    Ok(Some(match parse(&spec)? {
        Spec::Stub => return Ok(None),
        Spec::Ollama(model) => {
            tracing::info!("summaries: ollama model {model} at {}", ollama_url());
            Arc::new(
                noted_index::ollama::OllamaSummariser::new(&ollama_url(), model)
                    .map_err(|e| format!("could not build the Ollama summariser: {e}"))?,
            )
        }
        Spec::Gemini(model) => {
            tracing::info!("summaries: gemini model {model}");
            Arc::new(
                noted_index::gemini::GeminiSummariser::new(&gemini_key()?, model)
                    .map_err(|e| format!("could not build the Gemini summariser: {e}"))?,
            )
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognised_specs_parse() {
        assert!(matches!(parse("stub"), Ok(Spec::Stub)));
        assert!(matches!(parse("  stub  "), Ok(Spec::Stub)));
        assert!(matches!(parse("gemini:gemini-3.5-flash-lite"), Ok(Spec::Gemini(m)) if m == "gemini-3.5-flash-lite"));
        assert!(matches!(parse("ollama:llama3.2"), Ok(Spec::Ollama(m)) if m == "llama3.2"));
    }

    /// A model name containing a colon (`llama3.2:3b` is the normal Ollama tag
    /// form) must keep everything after the FIRST colon. `split(':')` with a
    /// two-element expectation would truncate the tag and silently pull a
    /// different model.
    #[test]
    fn a_model_name_may_contain_colons() {
        assert!(matches!(parse("ollama:llama3.2:3b"), Ok(Spec::Ollama(m)) if m == "llama3.2:3b"));
    }

    /// An empty value means "unset", not "malformed". `docker-compose.yml`
    /// always sets these variables, so `NOTED_EXTRACT=` in a `.env` — the
    /// obvious way to write "off" — would otherwise be a fatal startup error.
    #[test]
    fn an_empty_value_reads_as_unset() {
        // SAFETY: single-threaded test, and the var is removed straight after.
        unsafe { std::env::set_var("NOTED_TEST_EMPTY_SPEC", "   ") };
        assert_eq!(spec_for("NOTED_TEST_EMPTY_SPEC"), None);
        unsafe { std::env::set_var("NOTED_TEST_EMPTY_SPEC", "stub") };
        assert_eq!(spec_for("NOTED_TEST_EMPTY_SPEC").as_deref(), Some("stub"));
        unsafe { std::env::remove_var("NOTED_TEST_EMPTY_SPEC") };
        assert_eq!(spec_for("NOTED_TEST_EMPTY_SPEC"), None);
    }

    /// These must fail rather than fall back. A typo that degrades to a stub
    /// produces a deployment that looks healthy and answers with fiction.
    #[test]
    fn malformed_specs_are_rejected() {
        for bad in ["", "gemini", "gemini:", "openai:gpt-4", "ollama:", "  "] {
            assert!(parse(bad).is_err(), "{bad:?} should not have parsed");
        }
    }
}

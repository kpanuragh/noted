//! Real, LLM-backed providers for the two roles that were stub-only:
//! answer synthesis and community summarisation.
//!
//! Extraction already had one (`extract_providers::OllamaExtractor`); this
//! completes the set, so a deployment can configure all three roles
//! independently — they have genuinely different cost and latency profiles.
//! Extraction runs once per chunk over a whole corpus; synthesis runs once per
//! question with a human waiting.
//!
//! # THE NON-NEGOTIABLE: explicit timeouts
//!
//! `reqwest` applies NO request timeout by default. Without one, a model that
//! hangs — Ollama wedged loading weights, a GPU stuck, a server that accepted
//! the connection and went silent — blocks the await forever. No
//! consecutive-failure cap can help, because a call that never returns is never
//! counted as a failure. This is the single most likely first-contact failure
//! for a provider that has never run, and it is why both constants below exist
//! and why every constructor sets them.
//!
//! # What is NOT proven about this file
//!
//! It compiles, its prompt construction and response parsing are unit-tested
//! against recorded payloads, and its timeout behaviour is tested against a
//! listener that accepts and never answers. **No test here has talked to a real
//! model**, because this environment has none. The `#[ignore]`d tests in
//! `tests/ollama_live.rs` are the ones that would, and they carry the command
//! to run them. Until someone runs those against a real endpoint, treat this as
//! wiring that type-checks rather than as a verified integration.
use std::time::Duration;

use crate::answer::{AnswerError, AnswerProvider, AnswerRequest};
use crate::summary::{CommunityFacts, SummaryError, SummaryProvider};

/// Generous: local inference on CPU can legitimately take minutes for a long
/// context. A timeout tight enough to catch a hang quickly would turn
/// healthy-but-slow inference into a permanent failure, which is strictly worse
/// than the hang it prevents. The number only has to be shorter than "forever".
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Short: establishing a TCP connection is not inference. "Ollama is not
/// running" should surface in seconds rather than after five minutes.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

fn client(request: Duration, connect: Duration) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(request)
        .connect_timeout(connect)
        .build()
}

/// Ask Ollama's `/api/generate` for a completion.
async fn generate(
    http: &reqwest::Client,
    base_url: &str,
    model: &str,
    prompt: &str,
) -> Result<String, String> {
    let response = http
        .post(format!("{}/api/generate", base_url.trim_end_matches('/')))
        .json(&serde_json::json!({
            "model": model,
            "prompt": prompt,
            // Non-streaming: the callers here want one whole answer, and
            // streaming would mean reassembling it for no benefit.
            "stream": false,
        }))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("model returned HTTP {}", response.status()));
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("response was not JSON: {e}"))?;

    body.get("response")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "response JSON had no `response` field".to_string())
}

/// Synthesises answers over retrieved context.
pub struct OllamaAnswerer {
    base_url: String,
    model: String,
    model_id: String,
    http: reqwest::Client,
}

impl OllamaAnswerer {
    pub fn new(base_url: &str, model: &str) -> Result<Self, reqwest::Error> {
        Ok(Self {
            base_url: base_url.to_string(),
            model: model.to_string(),
            // Namespaced so a stub-built and a model-built artifact can never
            // be mistaken for one another.
            model_id: format!("ollama:{model}"),
            http: client(REQUEST_TIMEOUT, CONNECT_TIMEOUT)?,
        })
    }

    /// Constructor with the timeouts spelled out, so the hang behaviour is
    /// testable in seconds rather than minutes.
    pub fn with_timeouts(
        base_url: &str,
        model: &str,
        request: Duration,
        connect: Duration,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            base_url: base_url.to_string(),
            model: model.to_string(),
            model_id: format!("ollama:{model}"),
            http: client(request, connect)?,
        })
    }

    /// The prompt, built from retrieval rows.
    ///
    /// Public so a test can assert its shape WITHOUT a model — the part of this
    /// provider that can be verified here is exactly this.
    pub fn build_prompt(req: &AnswerRequest) -> String {
        let mut p = String::new();
        p.push_str(
            "Answer the question using ONLY the passages below. \
             If they do not contain the answer, say so plainly rather than guessing. \
             Do not invent sources.\n\n",
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
}

#[async_trait::async_trait]
impl AnswerProvider for OllamaAnswerer {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn synthesise(&self, req: &AnswerRequest) -> Result<String, AnswerError> {
        let prompt = Self::build_prompt(req);
        generate(&self.http, &self.base_url, &self.model, &prompt)
            .await
            .map(|s| s.trim().to_string())
            .map_err(AnswerError::Model)
    }
}

/// Writes community summaries.
pub struct OllamaSummariser {
    base_url: String,
    model: String,
    model_id: String,
    http: reqwest::Client,
}

impl OllamaSummariser {
    pub fn new(base_url: &str, model: &str) -> Result<Self, reqwest::Error> {
        Ok(Self {
            base_url: base_url.to_string(),
            model: model.to_string(),
            model_id: format!("ollama:{model}"),
            http: client(REQUEST_TIMEOUT, CONNECT_TIMEOUT)?,
        })
    }

    pub fn with_timeouts(
        base_url: &str,
        model: &str,
        request: Duration,
        connect: Duration,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            base_url: base_url.to_string(),
            model: model.to_string(),
            model_id: format!("ollama:{model}"),
            http: client(request, connect)?,
        })
    }

    pub fn build_prompt(facts: &CommunityFacts) -> String {
        let mut p = String::from(
            "Summarise what connects the following related items, in two or three \
             sentences. Describe the theme, not the list.\n\n",
        );
        for m in &facts.members {
            p.push_str(&format!("- {}", m.name));
            if let Some(d) = &m.description {
                p.push_str(&format!(": {d}"));
            }
            p.push('\n');
        }
        p.push_str("\nSummary:");
        p
    }
}

#[async_trait::async_trait]
impl SummaryProvider for OllamaSummariser {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn summarise(&self, facts: &CommunityFacts) -> Result<String, SummaryError> {
        let prompt = Self::build_prompt(facts);
        generate(&self.http, &self.base_url, &self.model, &prompt)
            .await
            .map(|s| s.trim().to_string())
            .map_err(SummaryError::Model)
    }
}

//! Gemini-backed providers for all three model roles: extraction, answer
//! synthesis and community summarisation.
//!
//! Why this exists alongside `ollama`: the two have opposite cost shapes.
//! Ollama is free but bound to whatever hardware the deployment has; Gemini is
//! billed per token but needs no GPU. A deployment picks per role, because the
//! roles themselves differ — extraction runs once per chunk over a whole corpus
//! (throughput, mechanical), synthesis runs once per question with a human
//! waiting (latency, judgement).
//!
//! # What was verified against the live API, and what was not
//!
//! Unlike `ollama`, the request and response shapes here were checked against
//! the real endpoint before this file was written, and three of them did not
//! match what the docs-from-memory would have produced:
//!
//!   * **Gemini's schema dialect is not JSON Schema.** Types are UPPERCASE
//!     (`OBJECT`, `ARRAY`, `STRING`, `NUMBER`) and nullability is `nullable:
//!     true` rather than a `["string", "null"]` union. `extract_providers`'
//!     schema — which is correct for Ollama — is rejected here, which is why
//!     [`response_schema`] is a separate definition rather than a shared one.
//!   * **`thinkingConfig.thinkingBudget: 0` is rejected with HTTP 400.** That is
//!     the older field. The accepted form is `thinkingLevel: "low"`, and it does
//!     work: it takes `gemini-3.6-flash` from 1010 thinking tokens on a trivial
//!     extraction to 0. Thinking bills as output, so on a per-chunk corpus job
//!     this is roughly a 4x cost difference for a task where it changed nothing.
//!   * **Truncation is not a parse failure, it is a `finishReason`.** A response
//!     cut short by the output limit comes back `200 OK` with `parts` PRESENT
//!     and `finishReason: MAX_TOKENS`. Under a `responseSchema` that yields
//!     syntactically invalid JSON; for prose it yields half an answer that looks
//!     like a whole one. See [`check_finish_reason`].
//!
//! What is still NOT proven: no test in this repository calls the live API
//! (they would need a key and would bill someone). The `#[ignore]`d tests in
//! `tests/gemini_live.rs` carry the command to run them.
//!
//! # The API key
//!
//! Sent as the `x-goog-api-key` HEADER, never as the `?key=` query parameter
//! the quickstarts use. A URL travels into access logs, proxy logs, and
//! `reqwest`'s own error `Display` output; a header does none of that. This is
//! also why the error paths below can pass `reqwest` errors through verbatim
//! without a redaction step that someone would eventually forget to apply.
//! [`Debug`] is implemented by hand for the same reason — the derived one would
//! print the key.
use std::fmt;
use std::time::Duration;

use crate::answer::{build_answer_prompt, AnswerError, AnswerProvider, AnswerRequest};
use crate::extract::{ExtractError, Extraction, ExtractionProvider};
use crate::summary::{build_summary_prompt, CommunityFacts, SummaryError, SummaryProvider};

/// Shorter than Ollama's 300s: this is a hosted service on someone else's
/// hardware, not a CPU grinding through weights locally. A request still
/// running after two minutes is wedged rather than slow.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Establishing a TCP connection is not inference — "the network is gone"
/// should surface in seconds.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Reads the key from the environment.
///
/// A free function rather than something the constructors do themselves, so a
/// caller that wants to fail at STARTUP on a missing key can, instead of
/// discovering it on the first user question.
pub fn api_key_from_env() -> Result<String, String> {
    match std::env::var("GEMINI_API_KEY") {
        Ok(k) if !k.trim().is_empty() => Ok(k),
        Ok(_) => Err("GEMINI_API_KEY is set but empty".to_string()),
        Err(_) => Err("GEMINI_API_KEY is not set".to_string()),
    }
}

/// A failed call, split by what the caller should DO about it.
///
/// Only two cases, because only two responses are distinguishable: retrying
/// this one thing later, versus stopping everything that would follow it. The
/// role-specific error enums each map these onto their own variants.
enum CallError {
    /// HTTP 429 — over the rate or quota limit. Every subsequent call will fail
    /// the same way until time passes, so callers must stop rather than
    /// continue.
    RateLimited(String),
    Other(String),
}

impl CallError {
    fn message(self) -> String {
        match self {
            Self::RateLimited(m) => format!("rate limited: {m}"),
            Self::Other(m) => m,
        }
    }
}

/// Everything a call needs, minus the prompt. One struct so the three providers
/// below cannot drift on timeouts, base URL, or key handling.
struct Endpoint {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    model_id: String,
}

// Hand-written: the derived impl would print `api_key`.
impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl Endpoint {
    fn new(
        base_url: &str,
        api_key: &str,
        model: &str,
        request: Duration,
        connect: Duration,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(request)
                .connect_timeout(connect)
                .build()?,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            // Namespaced, so a Gemini-built artifact and an Ollama- or
            // stub-built one can never be mistaken for one another. Every
            // content-addressed table in this crate keys off `model_id`.
            model_id: format!("gemini:{model}"),
            model: model.to_string(),
        })
    }

    /// One request/response round trip, with every failure the API can express
    /// mapped onto a distinct message.
    async fn generate(
        &self,
        prompt: &str,
        generation_config: serde_json::Value,
    ) -> Result<String, CallError> {
        let body = serde_json::json!({
            "contents": [{ "parts": [{ "text": prompt }] }],
            "generationConfig": generation_config,
        });

        let response = self
            .http
            .post(format!(
                "{}/models/{}:generateContent",
                self.base_url, self.model
            ))
            // Header, not `?key=` — see the module header.
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| CallError::Other(format!("request failed: {e}")))?;

        let status = response.status();
        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|e| CallError::Other(format!("response was not JSON: {e}")))?;

        if status.as_u16() == 429 {
            let detail = payload
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("no error message");
            return Err(CallError::RateLimited(detail.to_string()));
        }

        if !status.is_success() {
            // The API puts a usable sentence in `error.message`; surfacing the
            // bare status code instead would make a wrong model name and an
            // expired key look identical.
            let detail = payload
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("no error message");
            return Err(CallError::Other(format!("model returned HTTP {status}: {detail}")));
        }

        // An input rejected before generation: there are no candidates at all,
        // so the extraction below would otherwise report the unhelpful
        // "response contained no candidates".
        if let Some(reason) = payload
            .get("promptFeedback")
            .and_then(|f| f.get("blockReason"))
            .and_then(|r| r.as_str())
        {
            return Err(CallError::Other(format!("the prompt was blocked by the API: {reason}")));
        }

        let candidate = payload
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .ok_or_else(|| CallError::Other("response contained no candidates".to_string()))?;

        check_finish_reason(candidate.get("finishReason").and_then(|r| r.as_str()))
            .map_err(CallError::Other)?;

        // NOT `parts[0]` by index. A candidate that was blocked mid-generation
        // carries a `content` object with no `parts` key at all, and a
        // multi-part response puts the text after a leading part. Concatenating
        // every text part is correct for both.
        let text: String = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<String>()
            })
            .unwrap_or_default();

        if text.trim().is_empty() {
            return Err(CallError::Other("model returned no text".to_string()));
        }
        Ok(text)
    }
}

/// Maps a `finishReason` onto an error, or `Ok` if generation completed.
///
/// `STOP` is the only success. The rest are the reason this function exists at
/// all: the HTTP status is 200 and the body parses, so without an explicit check
/// a truncated or filtered generation is indistinguishable from a complete one.
/// Under a `responseSchema` a `MAX_TOKENS` cut produces invalid JSON and would
/// surface as a baffling parse error pointing at the model's output rather than
/// at the output limit; for a prose answer it produces a half-sentence that
/// reaches the user as though it were the whole answer.
///
/// `pub` so `tests/gemini.rs` can exercise every arm without a network call —
/// this is the part of the provider that is fully testable offline.
pub fn check_finish_reason(reason: Option<&str>) -> Result<(), String> {
    match reason {
        // Absent is treated as complete: the field is omitted on some
        // successful responses, and erroring on a missing optional field would
        // reject good generations.
        None | Some("STOP") => Ok(()),
        Some("MAX_TOKENS") => Err(
            "the model hit its output token limit and the response is truncated; \
             raise maxOutputTokens or shorten the input"
                .to_string(),
        ),
        Some("SAFETY") => Err("the response was blocked by the safety filter".to_string()),
        Some("RECITATION") => {
            Err("the response was blocked as a recitation of training data".to_string())
        }
        Some(other) => Err(format!("generation stopped unexpectedly: {other}")),
    }
}

/// The response schema handed to Gemini for extraction.
///
/// Deliberately NOT shared with `extract_providers::response_schema`, which is
/// correct for Ollama and rejected here: Gemini's dialect uppercases type names
/// and expresses nullability with `nullable: true` rather than a
/// `["string", "null"]` union. Two dialects genuinely means two definitions;
/// pretending otherwise would produce a schema neither backend accepts.
///
/// Structurally it must stay in step with [`Extraction`], which is what
/// `tests/gemini.rs` pins.
pub fn response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "OBJECT",
        "properties": {
            "entities": {
                "type": "ARRAY",
                "items": {
                    "type": "OBJECT",
                    "properties": {
                        "name": { "type": "STRING" },
                        "entity_type": { "type": "STRING" },
                        "description": { "type": "STRING", "nullable": true }
                    },
                    "required": ["name", "entity_type"]
                }
            },
            "edges": {
                "type": "ARRAY",
                "items": {
                    "type": "OBJECT",
                    "properties": {
                        "source": { "type": "STRING" },
                        "target": { "type": "STRING" },
                        "relation": { "type": "STRING" },
                        "weight": { "type": "NUMBER" }
                    },
                    "required": ["source", "target", "relation", "weight"]
                }
            }
        },
        "required": ["entities", "edges"]
    })
}

/// Extracts entities and edges from chunk text.
#[derive(Debug)]
pub struct GeminiExtractor {
    endpoint: Endpoint,
}

impl GeminiExtractor {
    pub fn new(api_key: &str, model: &str) -> Result<Self, reqwest::Error> {
        Self::with_timeouts(
            DEFAULT_BASE_URL,
            api_key,
            model,
            REQUEST_TIMEOUT,
            CONNECT_TIMEOUT,
        )
    }

    /// Base URL and timeouts spelled out, so a test can point this at a local
    /// listener and observe the hang behaviour in seconds rather than minutes.
    pub fn with_timeouts(
        base_url: &str,
        api_key: &str,
        model: &str,
        request: Duration,
        connect: Duration,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            endpoint: Endpoint::new(base_url, api_key, model, request, connect)?,
        })
    }

    pub fn build_prompt(text: &str) -> String {
        // Shared with the Ollama extractor — one prompt, one graph.
        crate::extract::build_extraction_prompt(text)
    }
}

#[async_trait::async_trait]
impl ExtractionProvider for GeminiExtractor {
    fn model_id(&self) -> &str {
        &self.endpoint.model_id
    }

    async fn extract(&self, text: &str) -> Result<Extraction, ExtractError> {
        let config = serde_json::json!({
            "responseMimeType": "application/json",
            "responseSchema": response_schema(),
            // Extraction is mechanical: reading names out of a sentence is not
            // a reasoning task, and the live check found the thinking budget
            // bought literally nothing on it while multiplying the billed
            // output. Accepted on models that reason; ignored by those that do
            // not, so it is safe to send unconditionally.
            "thinkingConfig": { "thinkingLevel": "low" },
        });

        let raw = self
            .endpoint
            .generate(&Self::build_prompt(text), config)
            .await
            .map_err(|e| match e {
                CallError::RateLimited(m) => ExtractError::RateLimited(m),
                CallError::Other(m) => ExtractError::Model(m),
            })?;

        parse_extraction(&raw)
    }
}

/// Parses the model's JSON into an [`Extraction`].
///
/// Separate from the request so it is testable without a network, and so the
/// recorded-payload tests exercise exactly the code the live path runs.
pub fn parse_extraction(raw: &str) -> Result<Extraction, ExtractError> {
    let wire: ExtractionWire = serde_json::from_str(raw)
        .map_err(|e| ExtractError::Invalid(format!("extraction was not the expected JSON: {e}")))?;

    // Blank names and blank edge endpoints are dropped by the SHARED
    // `sanitise` (see its docs: a blank name is graph corruption, not noise),
    // so both providers cannot drift on what counts as writable output.
    Ok(crate::extract::sanitise(Extraction {
        entities: wire
            .entities
            .into_iter()
            .map(|e| crate::extract::ExtractedEntity {
                name: e.name,
                entity_type: e.entity_type,
                description: e.description,
            })
            .collect(),
        edges: wire
            .edges
            .into_iter()
            .map(|e| crate::extract::ExtractedEdge {
                source: e.source,
                target: e.target,
                relation: e.relation,
                // Clamped, not rejected. An out-of-range confidence still
                // describes a real edge; discarding it would lose graph
                // structure over a formatting mistake. NaN is sent to 0.0
                // explicitly because `clamp` PANICS on a NaN bound.
                weight: if e.weight.is_nan() {
                    0.0
                } else {
                    e.weight.clamp(0.0, 1.0)
                },
            })
            .collect(),
    }))
}

#[derive(serde::Deserialize)]
struct ExtractionWire {
    #[serde(default)]
    entities: Vec<EntityWire>,
    #[serde(default)]
    edges: Vec<EdgeWire>,
}

#[derive(serde::Deserialize)]
struct EntityWire {
    name: String,
    entity_type: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(serde::Deserialize)]
struct EdgeWire {
    source: String,
    target: String,
    relation: String,
    weight: f32,
}

/// Synthesises answers over retrieved context.
#[derive(Debug)]
pub struct GeminiAnswerer {
    endpoint: Endpoint,
}

impl GeminiAnswerer {
    pub fn new(api_key: &str, model: &str) -> Result<Self, reqwest::Error> {
        Self::with_timeouts(
            DEFAULT_BASE_URL,
            api_key,
            model,
            REQUEST_TIMEOUT,
            CONNECT_TIMEOUT,
        )
    }

    pub fn with_timeouts(
        base_url: &str,
        api_key: &str,
        model: &str,
        request: Duration,
        connect: Duration,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            endpoint: Endpoint::new(base_url, api_key, model, request, connect)?,
        })
    }
}

#[async_trait::async_trait]
impl AnswerProvider for GeminiAnswerer {
    fn model_id(&self) -> &str {
        &self.endpoint.model_id
    }

    async fn synthesise(&self, req: &AnswerRequest) -> Result<String, AnswerError> {
        // No `thinkingLevel` override: this is the one role where reasoning is
        // the product. A human is waiting for a judgement over retrieved
        // passages, not for a field to be copied out of a sentence.
        self.endpoint
            .generate(&build_answer_prompt(req), serde_json::json!({}))
            .await
            .map(|s| s.trim().to_string())
            .map_err(|e| AnswerError::Model(e.message()))
    }
}

/// Writes community summaries.
#[derive(Debug)]
pub struct GeminiSummariser {
    endpoint: Endpoint,
}

impl GeminiSummariser {
    pub fn new(api_key: &str, model: &str) -> Result<Self, reqwest::Error> {
        Self::with_timeouts(
            DEFAULT_BASE_URL,
            api_key,
            model,
            REQUEST_TIMEOUT,
            CONNECT_TIMEOUT,
        )
    }

    pub fn with_timeouts(
        base_url: &str,
        api_key: &str,
        model: &str,
        request: Duration,
        connect: Duration,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            endpoint: Endpoint::new(base_url, api_key, model, request, connect)?,
        })
    }
}

#[async_trait::async_trait]
impl SummaryProvider for GeminiSummariser {
    fn model_id(&self) -> &str {
        &self.endpoint.model_id
    }

    async fn summarise(&self, facts: &CommunityFacts) -> Result<String, SummaryError> {
        self.endpoint
            .generate(&build_summary_prompt(facts), serde_json::json!({}))
            .await
            .map(|s| s.trim().to_string())
            .map_err(|e| SummaryError::Model(e.message()))
    }
}

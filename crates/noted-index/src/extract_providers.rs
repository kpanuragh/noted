//! Real (LLM-backed) `ExtractionProvider` implementations. Currently one:
//! `OllamaExtractor`, which talks to a local Ollama server's
//! `/api/generate` endpoint.
//!
//! Feature-gated behind `extract-ollama` (see `noted-index/Cargo.toml`) —
//! pulls in `reqwest` for a service that may not exist in every environment.
//! It does NOT in this one: **this module has never been run against a live
//! Ollama server.** What IS verified is that it compiles cleanly under
//! `cargo build -p noted-index --features extract-ollama` and that its
//! request-shaping and response-parsing/validation logic (`build_request`,
//! `parse_and_validate`) are exercised by the unit tests at the bottom of
//! this file, which are pure — no network, no Ollama. A real round trip
//! (does Ollama actually honour `format` this way, does a real small model's
//! output parse cleanly) is UNVERIFIED.
use crate::extract::{
    ExtractError, ExtractedEdge, ExtractedEntity, Extraction, ExtractionProvider,
};
use serde::{Deserialize, Serialize};

/// Talks to Ollama's `/api/generate` endpoint (see
/// <https://github.com/ollama/ollama/blob/main/docs/api.md#generate-a-completion>),
/// constraining the model's output to a JSON schema via the request's
/// `format` field.
///
/// This is the constrained-decoding lever that makes small local models
/// viable for extraction: Ollama grammar-constrains generation so the
/// response cannot help but validate against the schema, rather than the
/// provider hoping a "reply in JSON" instruction is enough — no free-text
/// wrapper, no fenced code block, no chatty preamble to strip before
/// parsing.
pub struct OllamaExtractor {
    base_url: String,
    model: String,
    // Namespaced (`ollama:{model}`) and computed once in `new`, rather than
    // formatted on every `model_id()` call, so the trait method (which
    // returns `&str`, not `String`) has something to borrow from without
    // resorting to a leak.
    model_id: String,
    client: reqwest::Client,
}

/// How long one `extract()` may take end-to-end before the request is torn
/// down.
///
/// reqwest applies NO request timeout by default. Without this, a model that
/// hangs (an Ollama process wedged loading weights, a GPU stuck, a server that
/// accepted the connection and then went silent) blocks `extract().await`
/// forever. `ExtractWorker::drain`'s `MAX_CONSECUTIVE_FAILURES` cap cannot
/// save it: that counts batches that RETURNED making no progress, and a batch
/// that never returns is never counted. The drain simply stops, with no error
/// and no log line.
///
/// 300s is chosen to be generous rather than tight. Extraction is a
/// constrained-decoding generation over a chunk of up to ~512 tokens on
/// whatever hardware the operator has; on a CPU-only box (which is what this
/// project has) a small local model can legitimately take minutes for a single
/// chunk, and a timeout that fires on healthy-but-slow inference would turn
/// every chunk into a poison chunk and stall the whole backfill — strictly
/// worse than the hang. The number only has to be shorter than "forever" to do
/// its job.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Establishing a TCP connection to a local (or LAN) Ollama is fast or not
/// happening at all — there is no slow-but-healthy case to protect, unlike
/// inference above. A short timeout means "Ollama isn't running" surfaces as a
/// clear error in seconds instead of waiting out `REQUEST_TIMEOUT`.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl OllamaExtractor {
    /// `base_url` example: `http://localhost:11434` (no trailing slash).
    /// `model` is an Ollama model tag, e.g. `qwen2.5:3b-instruct`.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_timeouts(base_url, model, REQUEST_TIMEOUT, CONNECT_TIMEOUT)
    }

    /// `new` with the timeouts spelled out. Exists so the hang behaviour is
    /// testable in bounded time — a test cannot wait out the 300s production
    /// `REQUEST_TIMEOUT` to prove the request is bounded at all.
    pub fn with_timeouts(
        base_url: impl Into<String>,
        model: impl Into<String>,
        request_timeout: std::time::Duration,
        connect_timeout: std::time::Duration,
    ) -> Self {
        let model = model.into();
        // Namespaced so this provider's extractions never collide with
        // `StubExtractor`'s (`stub-extractor-v1`) or another provider's
        // under the same Ollama model tag run through a different prompt —
        // `chunk_extractions`/`edges` key everything off `model_id`.
        let model_id = format!("ollama:{model}");
        Self {
            base_url: base_url.into(),
            model,
            model_id,
            // `build()` only fails on a TLS backend that cannot initialise;
            // there is no configuration here that can be invalid, and a
            // provider constructor returning Result for that would push an
            // impossible error onto every caller. Fall back to the
            // default-configured client rather than panicking — a client with
            // no timeout is still better than no client, and the failure it
            // would represent (broken TLS) has nothing to do with timeouts.
            client: reqwest::Client::builder()
                .timeout(request_timeout)
                .connect_timeout(connect_timeout)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

/// The JSON Schema handed to Ollama's `format` field. Kept structurally
/// identical to `Extraction`/`ExtractedEntity`/`ExtractedEdge` (see
/// `extract.rs`) so `parse_and_validate` never has to reconcile a schema
/// drift against the types it deserialises into.
fn response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "entities": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "entity_type": { "type": "string" },
                        "description": { "type": ["string", "null"] }
                    },
                    "required": ["name", "entity_type"]
                }
            },
            "edges": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "source": { "type": "string" },
                        "target": { "type": "string" },
                        "relation": { "type": "string" },
                        "weight": { "type": "number" }
                    },
                    "required": ["source", "target", "relation", "weight"]
                }
            }
        },
        "required": ["entities", "edges"]
    })
}

const PROMPT_PREAMBLE: &str = "You are an information-extraction engine. Read the note text below \
and extract the entities it mentions (people, places, concepts, organisations — anything worth a \
graph node) and the relations between them. Respond with ONLY the JSON object described by the \
schema: no prose, no markdown fences. If the text mentions nothing extractable, return empty \
\"entities\" and \"edges\" arrays rather than inventing content.\n\nText:\n";

#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: String,
    format: serde_json::Value,
    stream: bool,
}

#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
}

/// The wire shape parsed out of `GenerateResponse::response` — a plain,
/// unvalidated mirror of `Extraction`. Kept separate from `Extraction`
/// itself (rather than deriving `Deserialize` on it directly) because
/// `Extraction`/`ExtractedEdge` intentionally carry no serde derives in
/// `extract.rs` (they are an internal domain type, not a wire format) and
/// because this type is exactly where weight validation happens, between
/// "parsed" and "trusted".
#[derive(Deserialize)]
struct ExtractionWire {
    entities: Vec<EntityWire>,
    edges: Vec<EdgeWire>,
}

#[derive(Deserialize)]
struct EntityWire {
    name: String,
    entity_type: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct EdgeWire {
    source: String,
    target: String,
    relation: String,
    weight: f32,
}

fn build_request<'a>(model: &'a str, text: &str) -> GenerateRequest<'a> {
    GenerateRequest {
        model,
        prompt: format!("{PROMPT_PREAMBLE}{text}"),
        format: response_schema(),
        stream: false,
    }
}

/// Parse the model's raw JSON string and validate it into a trustworthy
/// `Extraction`.
///
/// VALIDATION DECISION: `edges.weight` is an unconstrained `real` column
/// (see migration `0005_graph.sql`) with no CHECK constraint, so nothing
/// downstream stops a NaN or +/-Inf weight reaching Postgres if this
/// provider didn't catch it. We REJECT (`ExtractError::Invalid`) rather than
/// clamp: `f32::clamp` does not fix NaN (`NaN.clamp(..)` is still NaN per
/// its own docs), so a clamp-based "fix" would need a separate NaN special
/// case anyway — at which point it is a silent substitution of a
/// hallucinated confidence value, not a correction of a merely-out-of-range
/// one. Rejecting instead surfaces as `ExtractError`, which
/// `extract_worker::process_batch` already treats as a poison chunk: logged
/// and skipped, left in the queue, never written to the graph. That is the
/// correct fate for output the schema constraint should have prevented but
/// evidently didn't.
fn parse_and_validate(raw: &str) -> Result<Extraction, ExtractError> {
    let wire: ExtractionWire = serde_json::from_str(raw)
        .map_err(|e| ExtractError::Invalid(format!("response was not valid JSON: {e}")))?;

    for edge in &wire.edges {
        if !edge.weight.is_finite() {
            return Err(ExtractError::Invalid(format!(
                "edge {} -> {} ({}) has a non-finite weight ({}); refusing to write it",
                edge.source, edge.target, edge.relation, edge.weight
            )));
        }
    }

    // `sanitise` drops blank-named entities and edges with a blank endpoint —
    // see its docs for why a blank name is graph corruption rather than noise.
    Ok(crate::extract::sanitise(Extraction {
        entities: wire
            .entities
            .into_iter()
            .map(|e| ExtractedEntity {
                name: e.name,
                entity_type: e.entity_type,
                description: e.description,
            })
            .collect(),
        edges: wire
            .edges
            .into_iter()
            .map(|e| ExtractedEdge {
                source: e.source,
                target: e.target,
                relation: e.relation,
                weight: e.weight,
            })
            .collect(),
    }))
}

#[async_trait::async_trait]
impl ExtractionProvider for OllamaExtractor {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn extract(&self, text: &str) -> Result<Extraction, ExtractError> {
        let request = build_request(&self.model, text);
        let url = format!("{}/api/generate", self.base_url);

        let resp = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| ExtractError::Model(format!("could not reach Ollama at {url}: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ExtractError::Model(format!(
                "Ollama returned {status}: {body}"
            )));
        }

        let parsed: GenerateResponse = resp
            .json()
            .await
            .map_err(|e| ExtractError::Model(format!("could not decode Ollama response: {e}")))?;

        parse_and_validate(&parsed.response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_embeds_the_schema_and_disables_streaming() {
        let req = build_request("qwen2.5:3b-instruct", "Alice met Bob.");
        assert_eq!(req.model, "qwen2.5:3b-instruct");
        assert!(req.prompt.contains("Alice met Bob."));
        assert!(!req.stream, "extraction reads the whole response at once");
        assert_eq!(req.format["required"][0], "entities");
    }

    #[test]
    fn parse_and_validate_accepts_well_formed_output() {
        let raw = r#"{
            "entities": [{"name": "Alice", "entity_type": "PERSON", "description": null}],
            "edges": [{"source": "Alice", "target": "Bob", "relation": "met", "weight": 0.9}]
        }"#;
        let ex = parse_and_validate(raw).unwrap();
        assert_eq!(ex.entities.len(), 1);
        assert_eq!(ex.edges.len(), 1);
        assert_eq!(ex.edges[0].weight, 0.9);
    }

    #[test]
    fn parse_and_validate_rejects_non_finite_weight() {
        // JSON has no literal for infinity, but a numeric literal that
        // overflows f64/f32 during parsing (well past f32::MAX) decodes to
        // +inf — a real shape a sloppy/miscalibrated model could emit as an
        // ordinary-looking number, unlike a bare `NaN` token which is not
        // valid JSON at all and would fail earlier, in `serde_json::from_str`
        // itself rather than in the `is_finite` check this test targets.
        let raw = r#"{
            "entities": [],
            "edges": [{"source": "A", "target": "B", "relation": "r", "weight": 1e400}]
        }"#;
        let err = parse_and_validate(raw).unwrap_err();
        assert!(matches!(err, ExtractError::Invalid(_)));
    }

    #[test]
    fn parse_and_validate_rejects_malformed_json() {
        let err = parse_and_validate("not json").unwrap_err();
        assert!(matches!(err, ExtractError::Invalid(_)));
    }

    /// A model that accepts the connection and then goes silent must make
    /// `extract()` RETURN (as an error the worker can treat as a poison
    /// chunk), not hang. reqwest has no default request timeout, so without an
    /// explicit one this test never finishes — `drain()` would stall forever
    /// and `MAX_CONSECUTIVE_FAILURES` would never trip, because a batch that
    /// never returns is never counted as a failed batch.
    #[tokio::test]
    async fn a_hung_server_times_out_instead_of_blocking_forever() {
        // Accepts connections and then never writes a response.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream); // hold it open, answer nothing
            }
        });

        let provider = OllamaExtractor::with_timeouts(
            format!("http://{addr}"),
            "hung-model",
            std::time::Duration::from_millis(300),
            std::time::Duration::from_secs(5),
        );

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider.extract("Alice met Bob."),
        )
        .await
        .expect("extract() must return within the outer bound, not hang")
        .expect_err("a server that never responds must surface as an error");
        assert!(
            matches!(err, ExtractError::Model(_)),
            "a transport timeout is a model/transport failure, got: {err}"
        );
    }

    #[test]
    fn model_id_is_namespaced_by_the_ollama_model_tag() {
        let provider = OllamaExtractor::new("http://localhost:11434", "qwen2.5:3b-instruct");
        assert_eq!(provider.model_id(), "ollama:qwen2.5:3b-instruct");
    }
}

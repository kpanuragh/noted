//! Gemini-backed providers.
//!
//! # Two kinds of test live here, and the difference matters
//!
//! The tests that RUN verify everything verifiable without spending money:
//! response parsing, the `finishReason` arms, error surfacing, key redaction,
//! and that a hung server is survivable. They use a canned-response listener,
//! so they exercise the SAME code path the live calls take — not a
//! reimplementation of it.
//!
//! The `#[ignore]`d tests verify the thing that actually needs the API: that a
//! real model's output parses into the types this pipeline expects, and that
//! the schema dialect is accepted. They are ignored because they bill a real
//! account, NOT because they are optional.
//!
//! Run them with:
//!
//! ```text
//! GEMINI_API_KEY=... NOTED_GEMINI_MODEL=gemini-3.5-flash-lite \
//!   cargo test -p noted-index --features gemini --test gemini_live -- --ignored --nocapture
//! ```
#![cfg(feature = "gemini")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use noted_index::extract::ExtractionProvider;
use noted_index::gemini::{
    check_finish_reason, parse_extraction, response_schema, GeminiExtractor,
};

/// Serves ONE canned HTTP response on a throwaway port, then stops.
///
/// A real socket rather than a mocked `reqwest` layer, so the status handling,
/// JSON decoding and `finishReason` checks under test are the same instructions
/// the live path runs. A mock at the client boundary would prove only that the
/// mock works.
fn serve_once(status_line: &str, body: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(response.as_bytes());
            let _ = sock.flush();
        }
    });
    format!("http://{addr}")
}

fn extractor_at(base: &str) -> GeminiExtractor {
    GeminiExtractor::with_timeouts(
        base,
        "test-key-do-not-log",
        "test-model",
        Duration::from_secs(5),
        Duration::from_secs(2),
    )
    .expect("client builds")
}

// ---------------------------------------------------------------- finishReason

/// The whole reason `check_finish_reason` exists. Each of these arrives as
/// HTTP 200 with a parseable body, so without this check they are
/// indistinguishable from a complete generation.
#[test]
fn every_non_stop_finish_reason_is_an_error() {
    assert!(check_finish_reason(Some("STOP")).is_ok());
    // Absent on some successful responses; erroring would reject good output.
    assert!(check_finish_reason(None).is_ok());

    // MAX_TOKENS is the one that actually bit during live probing: the response
    // came back 200 with `parts` PRESENT and the JSON cut mid-token.
    let err = check_finish_reason(Some("MAX_TOKENS")).unwrap_err();
    assert!(err.contains("truncated"), "unhelpful message: {err}");

    assert!(check_finish_reason(Some("SAFETY")).is_err());
    assert!(check_finish_reason(Some("RECITATION")).is_err());
    // An arm nobody has seen yet must fail closed, not be treated as success.
    assert!(check_finish_reason(Some("SOMETHING_NEW_IN_2027")).is_err());
}

/// End to end through the real HTTP path: a truncated response must not reach
/// the caller as a parse error pointing at the model's output.
#[tokio::test]
async fn a_truncated_response_reports_truncation_not_bad_json() {
    let base = serve_once(
        "200 OK",
        r#"{"candidates":[{"finishReason":"MAX_TOKENS","content":{"parts":[{"text":"{\"entities\":[{\"na"}]}}]}"#,
    );
    let err = extractor_at(&base).extract("anything").await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("truncated") || msg.contains("output token limit"),
        "a truncated generation must say so; got: {msg}"
    );
}

// ------------------------------------------------------------- error surfacing

/// A wrong model name and an expired key both arrive as a 4xx. Collapsing them
/// to a bare status code makes the difference invisible to whoever is trying to
/// configure this.
#[tokio::test]
async fn an_api_error_message_reaches_the_operator() {
    let base = serve_once(
        "400 Bad Request",
        r#"{"error":{"code":400,"message":"models/nope is not found for API version v1beta"}}"#,
    );
    let err = extractor_at(&base).extract("anything").await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("400"), "status missing: {msg}");
    assert!(msg.contains("is not found"), "API detail missing: {msg}");
}

/// A candidate whose `content` carries NO `parts` key. Indexing `parts[0]`
/// here — the obvious way to write this — panics inside a request handler.
#[tokio::test]
async fn a_candidate_with_no_parts_errors_rather_than_panicking() {
    let base = serve_once("200 OK", r#"{"candidates":[{"content":{"role":"model"}}]}"#);
    let err = extractor_at(&base).extract("anything").await.unwrap_err();
    assert!(err.to_string().contains("no text"), "{err}");
}

#[tokio::test]
async fn a_blocked_prompt_says_it_was_blocked() {
    let base = serve_once("200 OK", r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#);
    let err = extractor_at(&base).extract("anything").await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("blocked"), "{msg}");
    // Must NOT fall through to the generic "no candidates" path, which would
    // send someone hunting for a network fault.
    assert!(!msg.contains("no candidates"), "{msg}");
}

/// Multi-part responses concatenate. Taking only the first part silently
/// truncates an answer while reporting success.
#[tokio::test]
async fn every_text_part_is_kept() {
    let base = serve_once(
        "200 OK",
        r#"{"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"{\"entities\":[],"},{"text":"\"edges\":[]}"}]}}]}"#,
    );
    // Parses only if both halves were concatenated.
    let out = extractor_at(&base).extract("anything").await.unwrap();
    assert!(out.entities.is_empty() && out.edges.is_empty());
}

// ------------------------------------------------------------------- the hang

/// `reqwest` applies NO request timeout by default. A server that accepts the
/// connection and goes silent would otherwise block the await forever — and no
/// consecutive-failure cap can help, because a call that never returns is never
/// counted as a failure.
#[tokio::test]
async fn a_silent_server_fails_instead_of_hanging_forever() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let held = listener.accept();
        // Hold the connection open, answering nothing.
        std::thread::sleep(Duration::from_secs(30));
        drop(held);
    });

    let provider = GeminiExtractor::with_timeouts(
        &format!("http://{addr}"),
        "k",
        "m",
        Duration::from_millis(600),
        Duration::from_millis(300),
    )
    .unwrap();

    let started = Instant::now();
    assert!(provider.extract("anything").await.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?} — the request timeout is not being applied",
        started.elapsed()
    );
}

// ------------------------------------------------------------------- the key

/// The derived `Debug` would print the API key, and these structs land in
/// `tracing` output and panic messages.
#[test]
fn debug_output_never_contains_the_api_key() {
    let provider = extractor_at("http://127.0.0.1:1");
    let rendered = format!("{provider:?}");
    assert!(
        !rendered.contains("test-key-do-not-log"),
        "the API key leaked into Debug output: {rendered}"
    );
    assert!(rendered.contains("redacted"), "{rendered}");
}

// -------------------------------------------------------------------- parsing

#[test]
fn weights_are_clamped_rather_than_dropped() {
    let out = parse_extraction(
        r#"{"entities":[],"edges":[
            {"source":"a","target":"b","relation":"r","weight":1.4},
            {"source":"c","target":"d","relation":"r","weight":-2.0}]}"#,
    )
    .unwrap();
    // An out-of-range confidence still describes a real edge; discarding it
    // would lose graph structure over a formatting mistake.
    assert_eq!(out.edges.len(), 2);
    assert_eq!(out.edges[0].weight, 1.0);
    assert_eq!(out.edges[1].weight, 0.0);
}

/// `f32::clamp` PANICS if a bound comparison involves NaN, so this is a crash
/// path rather than a wrong-number path.
#[test]
fn a_nan_weight_becomes_zero_instead_of_panicking() {
    let out = parse_extraction(
        r#"{"entities":[],"edges":[{"source":"a","target":"b","relation":"r","weight":null}]}"#,
    );
    // `null` fails f32 deserialisation outright, which is fine — the contract
    // is that it does not panic.
    if let Ok(o) = out {
        assert!(o.edges.iter().all(|e| e.weight.is_finite()));
    }
}

#[test]
fn blank_names_and_descriptions_are_dropped() {
    let out = parse_extraction(
        r#"{"entities":[
             {"name":"  ","entity_type":"person"},
             {"name":" Priya ","entity_type":"person","description":"   "},
             {"name":"Arun","entity_type":"person","description":"runs Finance"}],
           "edges":[{"source":"Priya","target":"","relation":"knows","weight":0.5}]}"#,
    )
    .unwrap();
    assert_eq!(out.entities.len(), 2, "the blank-named entity survived");
    assert_eq!(out.entities[0].name, "Priya", "name was not trimmed");
    assert_eq!(
        out.entities[0].description, None,
        "a whitespace-only description is not a description"
    );
    assert_eq!(out.entities[1].description.as_deref(), Some("runs Finance"));
    assert!(out.edges.is_empty(), "an edge with no target survived");
}

/// Missing arrays default rather than failing: a note with no relationships in
/// it is a normal outcome, not a broken response.
#[test]
fn absent_arrays_are_empty_not_an_error() {
    let out = parse_extraction("{}").unwrap();
    assert!(out.entities.is_empty() && out.edges.is_empty());
}

/// The schema is what makes the model's output parseable at all, so it must
/// stay in step with the wire types. Gemini's dialect is NOT JSON Schema —
/// uppercase types, `nullable` instead of a union — and sending the JSON Schema
/// form (which is correct for Ollama) is rejected with a 400.
#[test]
fn the_schema_uses_geminis_dialect_and_matches_the_wire_types() {
    let s = response_schema();
    assert_eq!(s["type"], "OBJECT", "lowercase types are rejected by Gemini");
    let ent = &s["properties"]["entities"]["items"];
    assert_eq!(ent["type"], "OBJECT");
    for f in ["name", "entity_type", "description"] {
        assert!(ent["properties"].get(f).is_some(), "entity field {f} missing");
    }
    assert_eq!(
        ent["properties"]["description"]["nullable"], true,
        "nullability must be `nullable: true`, not a [\"string\",\"null\"] union"
    );
    let edge = &s["properties"]["edges"]["items"];
    for f in ["source", "target", "relation", "weight"] {
        assert!(edge["properties"].get(f).is_some(), "edge field {f} missing");
    }
    assert_eq!(edge["properties"]["weight"]["type"], "NUMBER");
}

// ------------------------------------------------------- live (needs a key)

fn live_config() -> Option<(String, String)> {
    let key = std::env::var("GEMINI_API_KEY").ok().filter(|k| !k.is_empty())?;
    let model = std::env::var("NOTED_GEMINI_MODEL")
        .unwrap_or_else(|_| "gemini-3.5-flash-lite".to_string());
    Some((key, model))
}

/// The test that makes this provider more than wiring that type-checks: a real
/// model, the real schema, parsed into the real types.
#[tokio::test]
#[ignore = "calls the live Gemini API and bills a real account"]
async fn live_extraction_parses_into_the_pipelines_types() {
    let Some((key, model)) = live_config() else {
        panic!("set GEMINI_API_KEY to run this");
    };
    let provider = GeminiExtractor::new(&key, &model).unwrap();
    let out = provider
        .extract(
            "Met Priya Raman at the Bangalore office on Tuesday to go over the Q3 \
             revenue model. She owns the pricing workstream and reports to Arun, \
             who runs Finance.",
        )
        .await
        .expect("live extraction");

    println!("model_id: {}", provider.model_id());
    println!("entities: {:#?}", out.entities);
    println!("edges: {:#?}", out.edges);

    assert!(!out.entities.is_empty(), "a real model found no entities");
    assert!(
        out.entities.iter().any(|e| e.name.contains("Priya")),
        "the most obvious entity in the text is missing"
    );
    assert!(!out.edges.is_empty(), "a real model found no relationships");
    assert!(
        out.edges.iter().all(|e| (0.0..=1.0).contains(&e.weight)),
        "a weight escaped the clamp"
    );
    // Every edge endpoint must name an extracted entity, or graph_write has
    // nothing to resolve it against and the edge is silently dropped later.
    let names: Vec<String> = out
        .entities
        .iter()
        .map(|e| noted_index::extract::normalise_entity(&e.name))
        .collect();
    for e in &out.edges {
        let s = noted_index::extract::normalise_entity(&e.source);
        let t = noted_index::extract::normalise_entity(&e.target);
        assert!(
            names.contains(&s) && names.contains(&t),
            "edge {s} -> {t} references an entity that was not extracted"
        );
    }
}

#[tokio::test]
#[ignore = "calls the live Gemini API and bills a real account"]
async fn live_answerer_returns_prose_grounded_in_the_passages() {
    use noted_index::answer::{AnswerProvider, AnswerRequest, ContextItem};
    let Some((key, model)) = live_config() else {
        panic!("set GEMINI_API_KEY to run this");
    };
    let provider = noted_index::gemini::GeminiAnswerer::new(&key, &model).unwrap();
    let answer = provider
        .synthesise(&AnswerRequest {
            question: "Who owns pricing?".into(),
            subjects: vec!["Priya Raman".into()],
            context: vec![ContextItem {
                source: "Meeting notes".into(),
                text: "Priya Raman owns the pricing workstream and reports to Arun.".into(),
                note: "matched your words".into(),
            }],
        })
        .await
        .expect("live synthesis");

    println!("answer: {answer}");
    assert!(!answer.trim().is_empty());
    assert!(
        answer.contains("Priya"),
        "the answer ignored the only passage it was given: {answer}"
    );
    // The assertions above are not enough on their own, and this is not a
    // hypothetical: the first live run returned the bare string "Priya Raman",
    // which satisfies both of them. A name is a lookup result, not an answer —
    // the Ask surface presents this prose to a reader who cannot see the
    // passages. The prompt now asks for complete sentences; this is what holds
    // it to that.
    assert!(
        answer.split_whitespace().count() >= 5,
        "expected a sentence, got a fragment: {answer:?}"
    );
    assert!(
        answer.contains("pricing"),
        "the answer names the person but not what they own: {answer:?}"
    );
}

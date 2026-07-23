//! M6-1 — real inference providers.
//!
//! # Two kinds of test live here, and the difference matters
//!
//! The tests that RUN verify everything that can be verified without a model:
//! prompt construction, response parsing, and — most importantly — that a hung
//! server is survivable.
//!
//! The `#[ignore]`d tests verify the thing that actually needs a model: that a
//! real one's output parses into the types this pipeline expects. They are
//! ignored because CI has no model, NOT because they are optional. Until
//! someone runs them, this provider is wiring that type-checks.
//!
//! Run them against a real endpoint with:
//!
//! ```text
//! ollama serve &
//! ollama pull llama3.2
//! NOTED_OLLAMA_URL=http://localhost:11434 NOTED_OLLAMA_MODEL=llama3.2 \
//!   cargo test -p noted-index --features extract-ollama --test ollama_live -- --ignored --nocapture
//! ```
#![cfg(feature = "extract-ollama")]

use std::time::Duration;

use noted_index::answer::{AnswerProvider, AnswerRequest, ContextItem};
use noted_index::extract::ExtractionProvider;
use noted_index::ollama::{OllamaAnswerer, OllamaSummariser};
use noted_index::summary::{CommunityFacts, CommunityMember, SummaryProvider};

fn endpoint() -> (String, String) {
    (
        std::env::var("NOTED_OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".into()),
        std::env::var("NOTED_OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2".into()),
    )
}

fn sample_request() -> AnswerRequest {
    AnswerRequest {
        question: "why did the migration overrun?".into(),
        subjects: vec!["helios".into(), "postgres".into()],
        context: vec![ContextItem {
            source: "Helios retro".into(),
            text: "The migration overran because checkpoint storms competed with index builds."
                .into(),
            note: "found directly by search".into(),
        }],
    }
}

// ------------------------------------------------- verifiable without a model --

/// The prompt carries every passage, its source, and its provenance note.
///
/// This is the half of the provider that CAN be checked here: a model is not
/// needed to know whether the text handed to it contains what retrieval found.
#[test]
fn the_answer_prompt_carries_every_passage_and_its_provenance() {
    let prompt = OllamaAnswerer::build_prompt(&sample_request());

    assert!(prompt.contains("why did the migration overrun?"), "the question");
    assert!(prompt.contains("Helios retro"), "the source");
    assert!(prompt.contains("found directly by search"), "the provenance note");
    assert!(prompt.contains("checkpoint storms"), "the passage itself");
    assert!(
        prompt.contains("ONLY the passages"),
        "and an instruction not to answer from its own weights"
    );
}

#[test]
fn the_summary_prompt_lists_members_and_their_descriptions() {
    let facts = CommunityFacts {
        community_id: uuid::Uuid::new_v4(),
        level: 0,
        members: vec![
            CommunityMember {
                id: uuid::Uuid::new_v4(),
                name: "helios".into(),
                entity_type: "PROJECT".into(),
                description: Some("a database migration".into()),
            },
            CommunityMember {
                id: uuid::Uuid::new_v4(),
                name: "postgres".into(),
                entity_type: "CONCEPT".into(),
                description: None,
            },
        ],
    };
    let prompt = OllamaSummariser::build_prompt(&facts);
    assert!(prompt.contains("helios: a database migration"));
    assert!(prompt.contains("postgres"), "a member with no description still appears");
}

/// **A hung server does not hang the caller.**
///
/// This is the single most likely first-contact failure for a provider that has
/// never run: `reqwest` sets no request timeout by default, so a server that
/// accepts the connection and never replies blocks forever — and no
/// consecutive-failure cap can catch it, because a call that never returns is
/// never counted as a failure.
///
/// MECHANISM PROTECTED: the `.timeout()` in the client builder. Remove it and
/// this test hangs instead of failing, which is exactly the production
/// behaviour it exists to prevent.
#[tokio::test]
async fn a_server_that_never_answers_times_out_rather_than_hanging() {
    // A listener that accepts and then says nothing at all.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            // Hold the connection open, reply never.
            std::mem::forget(stream);
        }
    });

    let answerer = OllamaAnswerer::with_timeouts(
        &format!("http://{addr}"),
        "no-such-model",
        Duration::from_millis(300),
        Duration::from_secs(5),
    )
    .unwrap();

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        answerer.synthesise(&sample_request()),
    )
    .await;

    assert!(
        result.is_ok(),
        "the provider must time out on its own, not be rescued by the outer bound"
    );
    assert!(result.unwrap().is_err(), "a hung server must surface as an error");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "it must give up promptly, took {:?}",
        started.elapsed()
    );
}

/// An unreachable endpoint fails fast on the CONNECT timeout rather than
/// waiting out the (much longer) request timeout.
#[tokio::test]
async fn an_unreachable_endpoint_fails_quickly() {
    let answerer = OllamaAnswerer::with_timeouts(
        // Reserved-for-documentation address: routable, never answers.
        "http://192.0.2.1:11434",
        "m",
        Duration::from_secs(300),
        Duration::from_millis(400),
    )
    .unwrap();

    let started = std::time::Instant::now();
    let out = tokio::time::timeout(Duration::from_secs(20), answerer.synthesise(&sample_request()))
        .await
        .expect("the connect timeout must fire well inside the request timeout");
    assert!(out.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "connect failure must not wait out REQUEST_TIMEOUT, took {:?}",
        started.elapsed()
    );
}

// ------------------------------------------ needs a real model (`--ignored`) --

/// **THE acceptance criterion: a real model's output parses into `Extraction`.**
///
/// Everything about the knowledge graph is proven against `StubExtractor`,
/// which emits single-token names, one entity type and one relation. Whether a
/// real model's JSON survives `serde` is the one thing that stub can never
/// tell us, and it is why this issue exists.
#[tokio::test]
#[ignore = "needs a running Ollama; see the module header for the command"]
async fn a_real_model_produces_output_that_parses_into_an_extraction() {
    let (url, model) = endpoint();
    let extractor = noted_index::extract_providers::OllamaExtractor::new(&url, &model);

    let text = "Alice and Bob reviewed the Postgres configuration for the Helios migration. \
                Bob linked the delay to checkpoint storms Alice had flagged.";
    let extraction = extractor
        .extract(text)
        .await
        .expect("a real model's output must parse into Extraction");

    assert!(
        !extraction.entities.is_empty(),
        "a real model must find at least one entity in that sentence"
    );
    eprintln!(
        "entities: {:?}",
        extraction.entities.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    eprintln!("edges: {}", extraction.edges.len());
}

/// A real model answers from the passages it was given.
#[tokio::test]
#[ignore = "needs a running Ollama; see the module header for the command"]
async fn a_real_model_answers_from_the_supplied_context() {
    let (url, model) = endpoint();
    let answerer = OllamaAnswerer::new(&url, &model).unwrap();

    let answer = answerer.synthesise(&sample_request()).await.expect("model call failed");
    assert!(!answer.trim().is_empty(), "an empty answer is refused upstream");
    eprintln!("answer: {answer}");
    assert!(
        answer.to_lowercase().contains("checkpoint") || answer.to_lowercase().contains("index"),
        "the answer should draw on the passage it was given, got: {answer}"
    );
}

/// A real model writes a usable community summary.
#[tokio::test]
#[ignore = "needs a running Ollama; see the module header for the command"]
async fn a_real_model_writes_a_community_summary() {
    let (url, model) = endpoint();
    let summariser = OllamaSummariser::new(&url, &model).unwrap();

    let facts = CommunityFacts {
        community_id: uuid::Uuid::new_v4(),
        level: 0,
        members: vec![
            CommunityMember {
                id: uuid::Uuid::new_v4(),
                name: "helios".into(),
                entity_type: "PROJECT".into(),
                description: Some("a database migration that overran".into()),
            },
            CommunityMember {
                id: uuid::Uuid::new_v4(),
                name: "checkpoint storms".into(),
                entity_type: "CONCEPT".into(),
                description: None,
            },
        ],
    };
    let summary = summariser.summarise(&facts).await.expect("model call failed");
    assert!(!summary.trim().is_empty());
    eprintln!("summary: {summary}");
}

/// **Does `incremental == full rebuild` still hold under a NON-deterministic
/// extractor?**
///
/// The M2a convergence property is a property of a deterministic pipeline. The
/// docs say plainly that a real model re-reading the same chunk will disagree
/// with itself, so the invariant cannot hold in production — but that was
/// reasoning, not measurement. This measures it: extract the same text twice
/// and report how much the two runs differ.
///
/// It asserts almost nothing on purpose. The point is the NUMBER, printed for a
/// human to read, because "how non-deterministic is it" is the input to
/// deciding whether the graph needs a stability strategy at all.
#[tokio::test]
#[ignore = "needs a running Ollama; see the module header for the command"]
async fn measure_how_much_a_real_model_disagrees_with_itself() {
    let (url, model) = endpoint();
    let extractor = noted_index::extract_providers::OllamaExtractor::new(&url, &model);
    let text = "Alice and Bob reviewed the Postgres configuration for the Helios migration.";

    let a = extractor.extract(text).await.expect("first extraction");
    let b = extractor.extract(text).await.expect("second extraction");

    let names = |e: &noted_index::extract::Extraction| {
        let mut v: Vec<String> = e.entities.iter().map(|x| x.name.to_lowercase()).collect();
        v.sort();
        v
    };
    let (na, nb) = (names(&a), names(&b));
    let shared = na.iter().filter(|n| nb.contains(n)).count();
    let union = na.len().max(nb.len()).max(1);

    eprintln!("run 1 entities: {na:?}");
    eprintln!("run 2 entities: {nb:?}");
    eprintln!(
        "agreement: {shared}/{union} = {:.0}%",
        100.0 * shared as f64 / union as f64
    );
    eprintln!(
        "NOTE: anything below 100% confirms that `incremental == full rebuild` \
         cannot hold under this model, which is what the M2a docs claim."
    );
}

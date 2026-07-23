//! Mirrors `crates/noted-index/tests/worker.rs` (M1b's embedding worker
//! tests) for the extraction worker. Stub-driven throughout — no LLM, no
//! network. Every provider here is deterministic and instant.
use noted_index::extract::{ExtractError, Extraction, ExtractionProvider, StubExtractor};
use noted_index::extract_worker::{BATCH_SIZE, ExtractWorker, ExtractWorkerError};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

/// Wraps `StubExtractor` with a call counter and a unique `model_id`.
///
/// `pending_extraction` CAN be workspace-scoped (`Some(workspace_id)`), and
/// every test below constructs its worker via `ExtractWorker::new_scoped` so
/// its poll only ever sees chunks live in its OWN fixture workspace — on a
/// live database shared by every test file in this crate (per this crate's
/// test notes), an unscoped poll would otherwise return every live chunk any
/// earlier test (in this run, in this file or another) ever created, not
/// just this test's own fixture. `extract()` always succeeds here regardless
/// (via `StubExtractor`), so that pollution would never have blocked a
/// drain — but a test asserting an EXACT extraction count must not count it
/// either. `calls` therefore ALSO only counts texts containing this
/// instance's unique `marker`, which every chunk THIS test seeds is tagged
/// with, as defense in depth: correct exact-count assertions even if a
/// worker is ever pointed at a wider scope than its own workspace.
struct CountingExtractor {
    model_id: String,
    marker: String,
    calls: AtomicUsize,
}

impl CountingExtractor {
    fn new(marker: impl Into<String>) -> Self {
        Self {
            model_id: format!("counting-extractor-{}", Uuid::new_v4()),
            marker: marker.into(),
            calls: AtomicUsize::new(0),
        }
    }
    fn chunks_extracted(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ExtractionProvider for CountingExtractor {
    fn model_id(&self) -> &str {
        &self.model_id
    }
    async fn extract(&self, text: &str) -> Result<Extraction, ExtractError> {
        if text.contains(&self.marker) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
        StubExtractor::new().extract(text).await
    }
}

/// Fails on one specific chunk text and succeeds (via the real stub logic)
/// on every other — a chunk the model genuinely cannot handle.
struct PoisonExtractor {
    bad: String,
    model_id: String,
}

#[async_trait::async_trait]
impl ExtractionProvider for PoisonExtractor {
    fn model_id(&self) -> &str {
        &self.model_id
    }
    async fn extract(&self, text: &str) -> Result<Extraction, ExtractError> {
        if text == self.bad {
            return Err(ExtractError::Model("this chunk cannot be extracted".into()));
        }
        StubExtractor::new().extract(text).await
    }
}

/// Counts every call and always reports a rate limit. The counter is what the
/// test is actually about: the failure mode being pinned is "how MANY requests
/// does one 429 cost", not "does a 429 produce an error".
struct RateLimitedExtractor {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ExtractionProvider for RateLimitedExtractor {
    fn model_id(&self) -> &str {
        "rate-limited-extractor"
    }
    async fn extract(&self, _text: &str) -> Result<Extraction, ExtractError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(ExtractError::RateLimited("quota exceeded".into()))
    }
}

async fn setup() -> (noted_db::PgPool, Uuid, Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('extract-worker-test') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let page: Uuid =
        sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .unwrap();
    (pool, ws, page)
}

/// Seed `n` live chunks (each with a capitalised word pair so the stub
/// extractor actually produces entities/edges, and `marker` so a
/// `CountingExtractor` can tell this test's own chunks apart from whatever
/// else is live in the shared test database) on a page.
async fn seed(pool: &noted_db::PgPool, page: Uuid, n: i32, marker: &str) {
    let mut hashes = Vec::new();
    for i in 0..n {
        let text = format!("Alpha{i} Beta{i} {marker}");
        let hash = format!("ewh-{}", Uuid::new_v4());
        noted_db::chunks::upsert(pool, &[(hash.clone(), text, 10)])
            .await
            .unwrap();
        hashes.push(hash);
    }
    noted_db::chunks::set_page_chunks(pool, page, &hashes)
        .await
        .unwrap();
}

#[tokio::test]
async fn drain_extracts_everything_then_stops() {
    let (pool, ws, page) = setup().await;
    let marker = Uuid::new_v4().to_string();
    seed(&pool, page, 5, &marker).await;
    let provider = Arc::new(CountingExtractor::new(marker));
    let w = ExtractWorker::new_scoped(pool.clone(), provider.clone(), ws);

    let n = w.drain().await.unwrap();
    assert!(n >= 5, "drain must extract all pending chunks, got {n}");

    // Draining again must do NOTHING — the queue is a set difference.
    let again = w.drain().await.unwrap();
    assert_eq!(again, 0, "a second drain must find no work");
}

/// The central invariant: after draining, extraction_progress reads 100% for
/// every live chunk under this model.
#[tokio::test]
async fn after_drain_extraction_progress_reaches_100_percent() {
    let (pool, ws, page) = setup().await;
    let marker = Uuid::new_v4().to_string();
    seed(&pool, page, 4, &marker).await;
    let provider = Arc::new(CountingExtractor::new(marker));
    let model_id = provider.model_id().to_string();
    let w = ExtractWorker::new_scoped(pool.clone(), provider, ws);
    w.drain().await.unwrap();

    let (extracted, total) = noted_db::graph::extraction_progress(&pool, &model_id, Some(ws))
        .await
        .unwrap();
    assert_eq!(
        extracted, total,
        "extraction_progress must read 100% after a full drain"
    );
    assert!(
        total >= 4,
        "progress must count the seeded chunks, got {total}"
    );
}

/// Crash-safety, proven by a real race: seed MORE than one batch's worth,
/// run exactly one `run_once()` (leaving a genuine second batch behind), then
/// resume with a fresh worker sharing the same provider/counter. THE
/// assertion is `==`, not `>=` — a `>=` would pass even if the resumed drain
/// re-extracted the first batch.
#[tokio::test]
async fn interrupted_work_resumes_without_duplication() {
    let (pool, ws, page) = setup().await;
    let marker = Uuid::new_v4().to_string();
    let total_seeded = BATCH_SIZE + 20;
    seed(&pool, page, total_seeded as i32, &marker).await;
    let provider = Arc::new(CountingExtractor::new(marker));
    let w = ExtractWorker::new_scoped(pool.clone(), provider.clone(), ws);

    // Simulate a crash after one batch by only running once.
    let first = w.run_once().await.unwrap();
    assert_eq!(
        first, BATCH_SIZE as usize,
        "the first run_once must extract exactly one full batch, proving work is left behind"
    );
    assert_eq!(
        provider.chunks_extracted() as i64,
        BATCH_SIZE,
        "exactly one batch's worth of chunks must have reached the provider"
    );

    // A "new worker" (same queue, same underlying provider/counter — standing
    // in for a fresh process that shares nothing but the database) finishes
    // the job.
    let w2 = ExtractWorker::new_scoped(pool.clone(), provider.clone(), ws);
    w2.drain().await.unwrap();

    assert_eq!(
        provider.chunks_extracted() as i64,
        total_seeded,
        "resuming must extract each chunk exactly once, never re-extracting the first batch"
    );
}

/// One chunk the provider can never extract must not stop the rest of the
/// backfill, but a queue that has become nothing-but-poison must still
/// terminate rather than spin forever.
#[tokio::test]
async fn a_poison_chunk_does_not_block_its_neighbours_and_drain_terminates() {
    let (pool, ws, page) = setup().await;
    let marker = Uuid::new_v4();
    let bad_text = format!("Poison {marker} UNEXTRACTABLE");

    let bad_hash = format!("ewh-bad-{}", Uuid::new_v4());
    noted_db::chunks::upsert(&pool, &[(bad_hash.clone(), bad_text.clone(), 10)])
        .await
        .unwrap();
    let mut hashes = vec![bad_hash.clone()];

    let mut good_hashes = Vec::new();
    for i in 0..5 {
        let hash = format!("ewh-good-{}", Uuid::new_v4());
        let text = format!("Healthy{i} Chunk{i} {marker}");
        noted_db::chunks::upsert(&pool, &[(hash.clone(), text, 10)])
            .await
            .unwrap();
        hashes.push(hash.clone());
        good_hashes.push(hash);
    }
    noted_db::chunks::set_page_chunks(&pool, page, &hashes)
        .await
        .unwrap();

    let model = format!("poison-extract-{}", Uuid::new_v4());
    let provider = Arc::new(PoisonExtractor {
        bad: bad_text,
        model_id: model.clone(),
    });
    let w = ExtractWorker::new_scoped(pool.clone(), provider, ws);

    let err = w.drain().await.expect_err(
        "a chunk that can never be extracted must surface as an error, not a clean exit",
    );
    assert!(
        matches!(err, ExtractWorkerError::Stalled { .. }),
        "a stalled drain must say so, got: {err}"
    );

    for hash in &good_hashes {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM chunk_extractions WHERE content_hash = $1 AND model_id = $2",
        )
        .bind(hash)
        .bind(&model)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            n, 1,
            "a healthy chunk must be extracted even though it shared a batch with a poison chunk"
        );
    }

    let bad_n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chunk_extractions WHERE content_hash = $1 AND model_id = $2",
    )
    .bind(&bad_hash)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        bad_n, 0,
        "the poison chunk must not be marked extracted when it never was"
    );
}

/// The workspace subtlety: a chunk shared by pages in two different
/// workspaces must be extracted into BOTH workspaces' graphs from one
/// `extract()` call, and the `chunk_extractions` marker must be written
/// exactly once (it has no workspace column).
#[tokio::test]
async fn a_chunk_shared_across_workspaces_is_extracted_into_both_graphs() {
    let (pool, ws_a, page_a) = setup().await;
    let (_, ws_b, page_b) = setup().await;
    let run = Uuid::new_v4();

    let text = format!("Shared Marker {run}");
    let hash = format!("ewh-shared-{run}");
    noted_db::chunks::upsert(&pool, &[(hash.clone(), text, 10)])
        .await
        .unwrap();
    noted_db::chunks::set_page_chunks(&pool, page_a, &[hash.clone()])
        .await
        .unwrap();
    noted_db::chunks::set_page_chunks(&pool, page_b, &[hash.clone()])
        .await
        .unwrap();

    let provider = Arc::new(CountingExtractor::new(run.to_string()));
    let model_id = provider.model_id().to_string();
    // Scoped to ws_a only: pending_extraction just needs ONE referencing
    // workspace to surface the chunk (the poll filters `page_chunks` through
    // `pages.workspace_id = ws_a`, and page_a alone satisfies that). The
    // fan-out to every referencing workspace's graph (including ws_b) still
    // happens inside `process_batch` via the UNSCOPED
    // `graph::workspaces_for_chunk`, so this remains a real test of that
    // fan-out even with a scoped poll.
    let w = ExtractWorker::new_scoped(pool.clone(), provider.clone(), ws_a);
    w.drain().await.unwrap();
    assert_eq!(
        provider.chunks_extracted(),
        1,
        "the shared chunk's text must be sent to the provider exactly ONCE, not once per workspace"
    );

    let a_edges: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM edges e
         JOIN entities se ON se.id = e.source_entity
         WHERE e.workspace_id = $1 AND e.source_chunk_hash = $2 AND e.model_id = $3",
    )
    .bind(ws_a)
    .bind(&hash)
    .bind(&model_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        a_edges > 0,
        "workspace A must have its own edges from the shared chunk"
    );

    let b_edges: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM edges e
         JOIN entities se ON se.id = e.source_entity
         WHERE e.workspace_id = $1 AND e.source_chunk_hash = $2 AND e.model_id = $3",
    )
    .bind(ws_b)
    .bind(&hash)
    .bind(&model_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        b_edges > 0,
        "workspace B must have its own edges from the shared chunk"
    );

    let marker_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chunk_extractions WHERE content_hash = $1 AND model_id = $2",
    )
    .bind(&hash)
    .bind(&model_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        marker_count, 2,
        "the marker is per-workspace: each of the two workspaces referencing this shared chunk \
         must have its own chunk_extractions row"
    );
}

/// The LATE-JOINER case, and the one the test above cannot see.
///
/// `a_chunk_shared_across_workspaces_is_extracted_into_both_graphs` wires BOTH
/// pages up before draining, so a single `workspaces_for_chunk` fan-out covers
/// both. Real workspaces do not appear simultaneously: workspace A drains to
/// completion, and only LATER does workspace B create a page whose text is
/// byte-identical. The chunk is content-addressed, so B's page reuses A's
/// chunk row — and if the extraction marker were keyed on `(content_hash,
/// model_id)` alone, that chunk would already be marked done and B would never
/// be queued. B would silently get NO graph while `extraction_progress(ws_b)`
/// cheerfully reported 1/1.
///
/// This is the invariant "a chunk shared across workspaces extracts into EACH
/// workspace's graph" applied across TIME, not just across one poll.
#[tokio::test]
async fn a_workspace_that_joins_a_shared_chunk_after_extraction_still_gets_a_graph() {
    let (pool, ws_a, page_a) = setup().await;
    let (_, ws_b, page_b) = setup().await;
    let run = Uuid::new_v4();

    let text = format!("Late Joiner {run}");
    let hash = format!("ewh-late-{run}");
    noted_db::chunks::upsert(&pool, &[(hash.clone(), text, 10)])
        .await
        .unwrap();

    // ---- Phase 1: workspace A alone, drained to completion ----
    noted_db::chunks::set_page_chunks(&pool, page_a, &[hash.clone()])
        .await
        .unwrap();
    let provider = Arc::new(CountingExtractor::new(run.to_string()));
    let model_id = provider.model_id().to_string();
    ExtractWorker::new_scoped(pool.clone(), provider.clone(), ws_a)
        .drain()
        .await
        .unwrap();

    let (extracted_a, total_a) = noted_db::graph::extraction_progress(&pool, &model_id, Some(ws_a))
        .await
        .unwrap();
    assert_eq!(
        (extracted_a, total_a),
        (1, 1),
        "sanity: workspace A must be fully extracted before B joins"
    );
    let a_edges = edge_set(&pool, ws_a, &hash, &model_id).await;
    assert!(
        !a_edges.is_empty(),
        "sanity: workspace A must have edges from its own extraction"
    );

    // ---- Phase 2: workspace B creates a page with byte-identical text ----
    // It reuses the very same content-addressed chunk row.
    noted_db::chunks::set_page_chunks(&pool, page_b, &[hash.clone()])
        .await
        .unwrap();

    let (extracted_b_before, total_b_before) =
        noted_db::graph::extraction_progress(&pool, &model_id, Some(ws_b))
            .await
            .unwrap();
    assert_eq!(
        (extracted_b_before, total_b_before),
        (0, 1),
        "workspace B has one live chunk and has extracted NOTHING; reporting it as already \
         extracted is the silent-no-graph failure"
    );

    ExtractWorker::new_scoped(pool.clone(), provider.clone(), ws_b)
        .drain()
        .await
        .unwrap();

    let b_edges = edge_set(&pool, ws_b, &hash, &model_id).await;
    assert_eq!(
        b_edges, a_edges,
        "workspace B must end up with the SAME edge set workspace A has for the shared chunk"
    );
}

/// `(source_name, target_name, relation)` for one workspace's edges from one
/// chunk under one model — the comparable shape of a per-workspace graph.
async fn edge_set(
    pool: &noted_db::PgPool,
    ws: Uuid,
    hash: &str,
    model_id: &str,
) -> std::collections::BTreeSet<(String, String, String)> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT se.name, te.name, e.relation
         FROM edges e
         JOIN entities se ON se.id = e.source_entity
         JOIN entities te ON te.id = e.target_entity
         WHERE e.workspace_id = $1 AND e.source_chunk_hash = $2 AND e.model_id = $3",
    )
    .bind(ws)
    .bind(hash)
    .bind(model_id)
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter().collect()
}


/// A rate limit must abandon the batch, not spend one refused request per chunk
/// learning the same fact repeatedly.
///
/// This is not hypothetical. Pointed at the live Gemini API on an exhausted
/// free tier, the worker retried four times in two seconds, each call burning
/// quota to be refused again — a backlog turning into a self-inflicted outage.
/// `ExtractError::RateLimited` exists solely so this path can be told apart
/// from a chunk the model merely could not handle, which SHOULD be skipped so
/// its neighbours still get processed.
#[tokio::test]
async fn a_rate_limit_abandons_the_batch_instead_of_burning_it() {
    let (pool, ws, page) = setup().await;
    seed(&pool, page, 8, &Uuid::new_v4().to_string()).await;

    let provider = Arc::new(RateLimitedExtractor {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let w = ExtractWorker::new_scoped(pool.clone(), provider.clone(), ws);

    // `drain` gives up after N no-progress batches, reporting `Stalled` rather
    // than a successful zero — a queue that cannot drain is not a drained
    // queue, and the distinction is the whole point of that error.
    let err = w.drain().await.expect_err("a fully rate-limited queue cannot drain");
    let msg = err.to_string();
    assert!(msg.contains("stalled"), "expected a stall, got: {msg}");

    let calls = provider.calls.load(std::sync::atomic::Ordering::SeqCst);
    // The precise bound matters more than the direction. Before the fix this
    // was one call per chunk per batch attempt; `drain` tolerates a few
    // no-progress batches before giving up, so a handful of calls is expected
    // — but never one per seeded chunk in a single pass.
    assert!(
        calls < 8,
        "one 429 cost {calls} requests across 8 chunks; the batch was not abandoned"
    );
    assert!(calls > 0, "the provider was never called at all");
}

/// The counterpart, and the reason `RateLimited` had to be a separate variant
/// rather than a message on `Model`: an ordinary model failure must still be
/// skipped so the chunks after it are processed. If the abandon-the-batch path
/// were reached for every error, one bad chunk would stall its whole batch.
#[tokio::test]
async fn an_ordinary_model_error_still_only_skips_its_own_chunk() {
    let (pool, ws, page) = setup().await;
    let marker = Uuid::new_v4().to_string();
    seed(&pool, page, 4, &marker).await;

    // Poison the text `seed` generates for i = 0.
    let provider = Arc::new(PoisonExtractor {
        bad: format!("Alpha0 Beta0 {marker}"),
        model_id: format!("poison-{marker}"),
    });
    let w = ExtractWorker::new_scoped(pool.clone(), provider.clone(), ws);

    // Ends in a stall too — the poison chunk never drains — but the number
    // that got through before that is what this test is about.
    let extracted = match w.drain().await {
        Ok(n) => n,
        Err(noted_index::extract_worker::ExtractWorkerError::Stalled { extracted, .. }) => {
            extracted
        }
        Err(e) => panic!("unexpected error: {e}"),
    };
    assert!(
        extracted >= 3,
        "one unextractable chunk stalled its neighbours: only {extracted} of 4 got through"
    );
}

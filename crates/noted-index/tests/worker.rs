use noted_index::provider::{EMBEDDING_DIMS, EmbedError, EmbeddingProvider};
use noted_index::worker::{BATCH_SIZE, BatchOutcome, Worker, WorkerError};
use std::sync::Arc;

/// Deterministic fake — no model download, no network. Counts calls so we can
/// prove work is not repeated.
struct Fake {
    dims: usize,
    calls: std::sync::atomic::AtomicUsize,
}

impl Fake {
    fn new(dims: usize) -> Self {
        Self {
            dims,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    fn texts_embedded(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for Fake {
    fn dimensions(&self) -> usize {
        self.dims
    }
    fn model_id(&self) -> &str {
        "fake-worker"
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls
            .fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; self.dims];
                v[0] = t.len() as f32; // deterministic and text-dependent
                v
            })
            .collect())
    }
}

/// Fails on one specific text and succeeds on every other — a chunk the model
/// genuinely cannot handle. The failure is batch-granular, as a real provider's
/// would be: the whole call errors if the bad text is anywhere in it.
struct Poison {
    bad: String,
    model_id: String,
}

#[async_trait::async_trait]
impl EmbeddingProvider for Poison {
    fn dimensions(&self) -> usize {
        EMBEDDING_DIMS
    }
    fn model_id(&self) -> &str {
        &self.model_id
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.iter().any(|t| *t == self.bad) {
            return Err(EmbedError::Model("this chunk cannot be embedded".into()));
        }
        Ok(texts
            .iter()
            .map(|_| vec![0.25f32; EMBEDDING_DIMS])
            .collect())
    }
}

async fn setup() -> (noted_db::PgPool, uuid::Uuid, uuid::Uuid) {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('worker-test') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    let page: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .unwrap();
    (pool, ws, page)
}

/// Seed `n` live chunks on a page. Goes through `page_chunks` — NOT `blocks`,
/// because chunk hashes and block hashes are different hash spaces that never
/// join. Seeding via blocks would make these tests pass against a `pending()`
/// that returns nothing in production.
async fn seed(pool: &noted_db::PgPool, page: uuid::Uuid, n: i32) {
    let mut hashes = Vec::new();
    for i in 0..n {
        let text = format!("unique chunk text number {i} {}", uuid::Uuid::new_v4());
        let hash = format!("wh-{}", uuid::Uuid::new_v4());
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
async fn a_dimension_mismatch_is_rejected_at_construction() {
    let (pool, _, _) = setup().await;
    let err = Worker::new(pool, Arc::new(Fake::new(1024)))
        .err()
        .expect("must reject");
    assert!(
        err.to_string().contains("768"),
        "error must name the schema dimension: {err}"
    );
}

#[tokio::test]
async fn drain_embeds_everything_then_stops() {
    let (pool, ws, page) = setup().await;
    seed(&pool, page, 5).await;
    let fake = Arc::new(Fake::new(EMBEDDING_DIMS));
    let w = Worker::new_scoped(pool.clone(), fake.clone(), ws).unwrap();

    let n = w.drain().await.unwrap();
    assert!(n >= 5, "drain must embed all pending chunks, got {n}");

    // Draining again must do NOTHING — the queue is a set difference.
    let again = w.drain().await.unwrap();
    assert_eq!(again, 0, "a second drain must find no work");
}

/// The central invariant: after draining, the embedded set equals the set of
/// LIVE chunk hashes. This mirrors M1a's "incremental converges to full
/// re-index" property.
#[tokio::test]
async fn after_drain_every_live_chunk_is_embedded() {
    let (pool, ws, page) = setup().await;
    seed(&pool, page, 4).await;
    let w = Worker::new_scoped(pool.clone(), Arc::new(Fake::new(EMBEDDING_DIMS)), ws).unwrap();
    w.drain().await.unwrap();

    // Scoped to `ws` (via the `pages` join) so this reads only THIS test's
    // own fixture, not every live chunk any other test in this binary has
    // ever created under the shared `fake-worker` model_id.
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
             SELECT DISTINCT pc.content_hash
             FROM page_chunks pc
             JOIN pages p ON p.id = pc.page_id
             WHERE p.workspace_id = $1
         ) pc
         LEFT JOIN embeddings e
           ON e.content_hash = pc.content_hash AND e.model_id = 'fake-worker'
         WHERE e.content_hash IS NULL",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        remaining, 0,
        "no live chunk may remain unembedded after drain"
    );

    let (embedded, total) = noted_db::chunks::progress(&pool, "fake-worker", Some(ws))
        .await
        .unwrap();
    assert_eq!(
        embedded, total,
        "progress must read 100% after a full drain"
    );
    assert!(
        total >= 4,
        "progress must count the seeded chunks, got {total}"
    );
}

/// Crash-safety: the queue has no "in progress" state, so a worker that dies
/// mid-drain leaves work for the next one and duplicates nothing.
///
/// This seeds MORE than `BATCH_SIZE` chunks so a single `run_once()` genuinely
/// cannot finish the job — there really is a second batch left over for a
/// "resumed" worker to pick up. A seed of `BATCH_SIZE` or fewer (as an earlier
/// version of this test did) lets the first `run_once()` embed everything in
/// one batch, leaving nothing to resume — at that point the test is just
/// `drain_embeds_everything_then_stops` under a different name.
#[tokio::test]
async fn interrupted_work_resumes_without_duplication() {
    let (pool, ws, page) = setup().await;
    let total_seeded = BATCH_SIZE + 20;
    seed(&pool, page, total_seeded as i32).await;
    let fake = Arc::new(Fake::new(EMBEDDING_DIMS));
    let w = Worker::new_scoped(pool.clone(), fake.clone(), ws).unwrap();

    // Simulate a crash after one batch by only running once.
    let first = w.run_once().await.unwrap();
    assert_eq!(
        first,
        BatchOutcome::Embedded(BATCH_SIZE as usize),
        "the first run_once must embed exactly one full batch, proving work is left behind"
    );
    assert_eq!(
        fake.texts_embedded() as i64,
        BATCH_SIZE,
        "exactly one batch's worth of texts must have reached the provider"
    );

    // A "new worker" (same queue, same underlying provider/counter — standing
    // in for a fresh process that shares nothing but the database) finishes
    // the job.
    let w2 = Worker::new_scoped(pool.clone(), fake.clone(), ws).unwrap();
    w2.drain().await.unwrap();

    // THE assertion: total texts ever sent to the provider must equal exactly
    // the number of chunks seeded, no more. `pending()` is a set difference
    // against `embeddings`, so if the resumed drain had re-embedded the first
    // batch (e.g. because `pending()` failed to exclude already-embedded rows,
    // or a worker re-read stale state), the provider's running counter would
    // exceed `total_seeded` and this `==` would fail. A `>=` here (as an
    // earlier version of this test used) would pass even if every chunk were
    // embedded twice — it only proves "at least everything got done", not
    // "nothing was redone". `==` is what actually proves no duplication.
    assert_eq!(
        fake.texts_embedded() as i64,
        total_seeded,
        "resuming must embed each chunk exactly once, never re-embedding the first batch"
    );

    let (embedded, all) = noted_db::chunks::progress(&pool, "fake-worker", Some(ws))
        .await
        .unwrap();
    assert_eq!(embedded, all, "progress must reach 100% after resuming");
}

/// One chunk the provider can never embed must not stop the backfill — and the
/// drain must still end rather than spin on it forever.
///
/// Both halves matter. Propagating the embed error (the old behaviour) aborted the
/// whole drain, so a single bad chunk blocked every other chunk on every restart,
/// forever. But merely swallowing the error would spin: the queue is a set
/// difference with no state, so an unembeddable chunk is handed back on every
/// single poll.
///
/// The poison chunk shares a batch with the healthy ones here (they are seeded
/// together and BATCH_SIZE is 64), which is the case that actually bites: batch-
/// granular isolation alone would let one bad chunk take its 63 innocent
/// neighbours down with it, permanently, because `pending()` is deterministic and
/// hands back the identical batch every time.
///
/// Termination is proven by this test returning at all — a spinning `drain` hangs
/// until the harness kills it.
#[tokio::test]
async fn a_poison_chunk_does_not_block_its_neighbours_and_drain_terminates() {
    let (pool, ws, page) = setup().await;
    let marker = uuid::Uuid::new_v4();
    let bad_text = format!("poison {marker} UNEMBEDDABLE");

    let bad_hash = format!("ph-bad-{}", uuid::Uuid::new_v4());
    noted_db::chunks::upsert(&pool, &[(bad_hash.clone(), bad_text.clone(), 10)])
        .await
        .unwrap();
    let mut hashes = vec![bad_hash.clone()];

    let mut good_hashes = Vec::new();
    for i in 0..5 {
        let hash = format!("ph-good-{}", uuid::Uuid::new_v4());
        let text = format!("healthy {marker} chunk {i}");
        noted_db::chunks::upsert(&pool, &[(hash.clone(), text, 10)])
            .await
            .unwrap();
        hashes.push(hash.clone());
        good_hashes.push(hash);
    }
    noted_db::chunks::set_page_chunks(&pool, page, &hashes)
        .await
        .unwrap();

    // A model_id nothing else uses, so this worker's embeddings are its own.
    // That uniqueness alone is NOT enough to isolate the `pending()` poll,
    // though: with a brand-new model_id, EVERY live chunk in the shared test
    // database has no embedding under it, so an unscoped worker would treat
    // every other test's fixture chunks as pending too. `new_scoped` to this
    // test's own workspace is what actually isolates it.
    let model = format!("fake-poison-{}", uuid::Uuid::new_v4());
    let provider = Arc::new(Poison {
        bad: bad_text,
        model_id: model.clone(),
    });
    let w = Worker::new_scoped(pool.clone(), provider, ws).unwrap();

    let err = w.drain().await.expect_err(
        "a chunk that can never be embedded must surface as an error, not a clean exit",
    );
    assert!(
        matches!(err, WorkerError::Stalled { .. }),
        "a stalled drain must say so, got: {err}"
    );

    // THE assertion: the healthy chunks that shared the poison's batch got embedded
    // regardless. Batch-granular isolation alone fails right here.
    for hash in &good_hashes {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM embeddings WHERE content_hash = $1 AND model_id = $2",
        )
        .bind(hash)
        .bind(&model)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            n, 1,
            "a healthy chunk must be embedded even though it shared a batch with a poison chunk"
        );
    }

    let bad: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM embeddings WHERE content_hash = $1 AND model_id = $2",
    )
    .bind(&bad_hash)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        bad, 0,
        "the poison chunk must not get an embedding it never produced"
    );
}

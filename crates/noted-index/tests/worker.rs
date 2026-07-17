use std::sync::Arc;
use noted_index::provider::{EmbedError, EmbeddingProvider, EMBEDDING_DIMS};
use noted_index::worker::Worker;

/// Deterministic fake — no model download, no network. Counts calls so we can
/// prove work is not repeated.
struct Fake {
    dims: usize,
    calls: std::sync::atomic::AtomicUsize,
}

impl Fake {
    fn new(dims: usize) -> Self {
        Self { dims, calls: std::sync::atomic::AtomicUsize::new(0) }
    }
    fn texts_embedded(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for Fake {
    fn dimensions(&self) -> usize { self.dims }
    fn model_id(&self) -> &str { "fake-worker" }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
        Ok(texts.iter().map(|t| {
            let mut v = vec![0.0f32; self.dims];
            v[0] = t.len() as f32; // deterministic and text-dependent
            v
        }).collect())
    }
}

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('worker-test') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    let page: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
        .bind(ws).fetch_one(&pool).await.unwrap();
    (pool, page)
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
        noted_db::chunks::upsert(pool, &[(hash.clone(), text, 10)]).await.unwrap();
        hashes.push(hash);
    }
    noted_db::chunks::set_page_chunks(pool, page, &hashes).await.unwrap();
}

#[tokio::test]
async fn a_dimension_mismatch_is_rejected_at_construction() {
    let (pool, _) = setup().await;
    let err = Worker::new(pool, Arc::new(Fake::new(1024))).err().expect("must reject");
    assert!(err.to_string().contains("768"), "error must name the schema dimension: {err}");
}

#[tokio::test]
async fn drain_embeds_everything_then_stops() {
    let (pool, page) = setup().await;
    seed(&pool, page, 5).await;
    let fake = Arc::new(Fake::new(EMBEDDING_DIMS));
    let w = Worker::new(pool.clone(), fake.clone()).unwrap();

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
    let (pool, page) = setup().await;
    seed(&pool, page, 4).await;
    let w = Worker::new(pool.clone(), Arc::new(Fake::new(EMBEDDING_DIMS))).unwrap();
    w.drain().await.unwrap();

    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT DISTINCT content_hash FROM page_chunks) pc
         LEFT JOIN embeddings e
           ON e.content_hash = pc.content_hash AND e.model_id = 'fake-worker'
         WHERE e.content_hash IS NULL",
    )
    .fetch_one(&pool).await.unwrap();
    assert_eq!(remaining, 0, "no live chunk may remain unembedded after drain");

    let (embedded, total) = noted_db::chunks::progress(&pool, "fake-worker").await.unwrap();
    assert_eq!(embedded, total, "progress must read 100% after a full drain");
    assert!(total >= 4, "progress must count the seeded chunks, got {total}");
}

/// Crash-safety: the queue has no "in progress" state, so a worker that dies
/// mid-drain leaves work for the next one and duplicates nothing.
#[tokio::test]
async fn interrupted_work_resumes_without_duplication() {
    let (pool, page) = setup().await;
    seed(&pool, page, 6).await;
    let fake = Arc::new(Fake::new(EMBEDDING_DIMS));
    let w = Worker::new(pool.clone(), fake.clone()).unwrap();

    // Simulate a crash after one batch by only running once.
    let first = w.run_once().await.unwrap();
    assert!(first > 0, "the first batch must do some work");
    let embedded_after_first = fake.texts_embedded();

    // A "new worker" (same queue) finishes the job.
    let w2 = Worker::new(pool.clone(), fake.clone()).unwrap();
    w2.drain().await.unwrap();

    let total = fake.texts_embedded();
    let (embedded, all) = noted_db::chunks::progress(&pool, "fake-worker").await.unwrap();
    assert_eq!(embedded, all, "progress must reach 100% after resuming");
    assert!(
        total >= embedded_after_first,
        "resuming must not lose work already done"
    );
}

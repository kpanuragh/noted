//! M6-2 — the background indexer.
//!
//! Requires the `embed` feature (the scheduler drives the embedding worker).
#![cfg(feature = "embed")]

use std::sync::Arc;
use std::time::Duration;

use noted_index::provider::EmbeddingProvider;
use noted_index::scheduler::{IndexingStatus, Scheduler, status};
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

async fn workspace(pool: &noted_db::PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('sched-test') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A page with one chunk, exactly as the projection would leave it.
async fn page_with_chunk(pool: &noted_db::PgPool, ws: Uuid, text: &str) -> String {
    let page: Uuid =
        sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, 'Sched') RETURNING id")
            .bind(ws)
            .fetch_one(pool)
            .await
            .unwrap();
    let hash = format!("sched-{}", Uuid::new_v4());
    noted_db::chunks::upsert(pool, &[(hash.clone(), text.to_string(), 10)])
        .await
        .unwrap();
    noted_db::chunks::set_page_chunks(pool, page, &[hash.clone()])
        .await
        .unwrap();
    hash
}

/// A deterministic embedder with a unique model id per test, so each test owns
/// its own vector space (the HNSW recall discipline the rest of the suite uses).
struct StubEmbedder(String);

#[async_trait::async_trait]
impl EmbeddingProvider for StubEmbedder {
    fn dimensions(&self) -> usize {
        768
    }
    fn model_id(&self) -> &str {
        &self.0
    }
    async fn embed(
        &self,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, noted_index::provider::EmbedError> {
        Ok(texts.iter().map(|_| vec![0.1f32; 768]).collect())
    }
}

/// **The headline property: writing a page makes it searchable with NO human
/// running a binary.**
///
/// MECHANISM PROTECTED: the loop in `Scheduler::start`. Without it the chunk
/// stays unembedded forever and this times out.
#[tokio::test]
async fn a_new_chunk_is_indexed_without_anyone_running_the_cli() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = format!("sched-{}", Uuid::new_v4());
    let hash = page_with_chunk(&pool, ws, "the scheduler indexes this by itself").await;

    // Premise: it is NOT indexed yet. Without this the test could pass against
    // a chunk something else had already embedded.
    let before = status(&pool, &model, None, Some(ws)).await.unwrap();
    assert_eq!(
        (before.embedded, before.embed_total),
        (0, 1),
        "premise: exactly one chunk, not yet embedded"
    );

    let scheduler = Scheduler::start(
        pool.clone(),
        Arc::new(StubEmbedder(model.clone())),
        None,
    )
    .unwrap();

    // Poll rather than sleep-and-hope: a fixed sleep either flakes on a slow
    // machine or wastes time on a fast one.
    let mut indexed = false;
    for _ in 0..100 {
        let s = status(&pool, &model, None, Some(ws)).await.unwrap();
        if s.is_current() && s.embedded == 1 {
            indexed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    scheduler.stop().await;

    assert!(indexed, "the scheduler must have embedded the chunk on its own");

    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM embeddings WHERE content_hash = $1 AND model_id = $2")
            .bind(&hash)
            .bind(&model)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, 1, "and actually stored a vector for it");
}

/// `stop` really stops it — a scheduler that ignored shutdown would keep
/// writing during a deploy, and the test suite would leak background tasks into
/// every later test.
#[tokio::test]
async fn stopping_the_scheduler_actually_stops_it() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = format!("sched-{}", Uuid::new_v4());

    let scheduler =
        Scheduler::start(pool.clone(), Arc::new(StubEmbedder(model.clone())), None).unwrap();
    scheduler.stop().await;

    // Work arriving AFTER the stop must not be touched.
    page_with_chunk(&pool, ws, "written after the scheduler stopped").await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    let s = status(&pool, &model, None, Some(ws)).await.unwrap();
    assert_eq!(
        (s.embedded, s.embed_total),
        (0, 1),
        "a stopped scheduler must not keep indexing"
    );
}

/// Staleness is reported honestly rather than rounded to "fine".
#[tokio::test]
async fn status_reports_the_backlog_rather_than_claiming_to_be_current() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = format!("sched-{}", Uuid::new_v4());

    for i in 0..3 {
        page_with_chunk(&pool, ws, &format!("pending work {i}")).await;
    }

    let s = status(&pool, &model, None, Some(ws)).await.unwrap();
    assert_eq!(s.embed_total, 3);
    assert_eq!(s.embedded, 0);
    assert_eq!(s.pending(), 3, "the UI must be able to count this down");
    assert!(!s.is_current(), "three unindexed chunks is not 'current'");
}

/// With no extraction provider, extraction is 0-of-0 — not "everything
/// pending". A backlog nothing will ever drain would leave `is_current` false
/// forever and the UI permanently alarmed.
#[tokio::test]
async fn no_extraction_provider_means_no_extraction_backlog() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = format!("sched-{}", Uuid::new_v4());
    page_with_chunk(&pool, ws, "some text").await;

    let s = status(&pool, &model, None, Some(ws)).await.unwrap();
    assert_eq!((s.extracted, s.extract_total), (0, 0));

    // And with one configured, the same chunk IS pending extraction — which is
    // what proves the zero above came from the provider being absent rather
    // than from the query finding nothing.
    let with = status(&pool, &model, Some("some-extractor"), Some(ws))
        .await
        .unwrap();
    assert_eq!(with.extract_total, 1);
    assert_eq!(with.extracted, 0);
}

#[test]
fn is_current_and_pending_agree_on_the_boundary() {
    let done = IndexingStatus {
        embedded: 5,
        embed_total: 5,
        extracted: 2,
        extract_total: 2,
    };
    assert!(done.is_current());
    assert_eq!(done.pending(), 0);

    let behind = IndexingStatus {
        embedded: 4,
        embed_total: 5,
        extracted: 0,
        extract_total: 2,
    };
    assert!(!behind.is_current());
    assert_eq!(behind.pending(), 3);
}

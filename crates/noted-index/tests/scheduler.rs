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
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
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
        Scheduler::start(pool.clone(), Arc::new(StubEmbedder(model.clone())), None, None).unwrap();
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

/// A deterministic summariser that records how many times it was called, so a
/// test can tell "the scheduler summarised" from "a summary happened to exist".
struct CountingSummariser {
    model_id: String,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl noted_index::summary::SummaryProvider for CountingSummariser {
    fn model_id(&self) -> &str {
        &self.model_id
    }
    async fn summarise(
        &self,
        facts: &noted_index::summary::CommunityFacts,
    ) -> Result<String, noted_index::summary::SummaryError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(format!("A theme covering {} items.", facts.members.len()))
    }
}

/// **A clustered workspace gets its themes summarised with NO human running a
/// binary** — the same property the embedding test asserts, for the other half
/// of the pipeline.
///
/// MECHANISM PROTECTED: the summary stage of `run_pass`. Without it the
/// scheduler embeds and extracts but never writes a summary, so `communities`
/// fills up while `community_summaries` stays empty and "ask across everything"
/// answers "this workspace has no summarised themes yet" forever. That was the
/// state of every deployment before this stage existed: `SummaryWorker` was
/// reachable only from the CLI and from global search's lazy REFRESH, which
/// regenerates a stale summary and never creates a missing one.
#[tokio::test]
async fn the_scheduler_summarises_communities_without_the_cli() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = format!("sched-sum-{}", Uuid::new_v4());

    // A community with members, exactly as CommunityWorker would leave it —
    // but backdated, and that is not decoration.
    //
    // The scheduler is INSTANCE-WIDE and this database is shared. A fresh
    // model id makes every community on the instance pending (a model change is
    // a full regeneration), which here is 1800+ workspaces, and one pass serves
    // only SUMMARY_WORKSPACES_PER_PASS of them in `created_at` order. A
    // just-created workspace sorts last and would never be reached, so the test
    // would fail while the mechanism worked perfectly.
    //
    // Backdating puts this workspace at the head of the queue, which is the
    // test controlling its own position rather than hoping. Asserting an
    // instance-wide property against a shared database without doing this is
    // the scar `materialize.rs` already carries.
    let community: Uuid = sqlx::query_scalar(
        "INSERT INTO communities (workspace_id, level, member_set_hash, created_at)
         VALUES ($1, 0, $2, TIMESTAMPTZ '1970-01-01') RETURNING id",
    )
    .bind(ws)
    .bind(format!("hash-{}", Uuid::new_v4()))
    .fetch_one(&pool)
    .await
    .unwrap();
    for name in ["alpha", "beta", "gamma"] {
        let e = noted_db::graph::resolve_entity(
            &pool,
            ws,
            &format!("{name}-{}", Uuid::new_v4()),
            Some("CONCEPT"),
            None,
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO community_members (community_id, entity_id) VALUES ($1, $2)")
            .bind(community)
            .bind(e)
            .execute(&pool)
            .await
            .unwrap();
    }

    // Premise: no summary yet, or this proves nothing.
    let before: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM community_summaries WHERE community_id = $1",
    )
    .bind(community)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before, 0, "premise: the community starts unsummarised");

    let summariser = Arc::new(CountingSummariser {
        model_id: model.clone(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let scheduler = Scheduler::start(
        pool.clone(),
        Arc::new(StubEmbedder(model.clone())),
        None,
        Some(summariser.clone()),
    )
    .unwrap();

    let mut summarised = false;
    let mut embedded = false;
    for _ in 0..100 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM community_summaries WHERE community_id = $1 AND model_id = $2",
        )
        .bind(community)
        .bind(&model)
        .fetch_one(&pool)
        .await
        .unwrap();
        if n == 1 {
            summarised = true;
            let e: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM community_summary_embeddings WHERE community_id = $1",
            )
            .bind(community)
            .fetch_one(&pool)
            .await
            .unwrap();
            if e == 1 {
                embedded = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    scheduler.stop().await;

    // CLEAN UP THE BACKDATING BEFORE ASSERTING.
    //
    // The `1970-01-01` above is not inert test data: this database is shared
    // with the running application, and a backdated community sits at the HEAD
    // of the instance-wide summary queue for as long as it exists. Left behind,
    // every run of this test pins one more workspace in front of real user
    // data — which is exactly what happened, and it starved a real workspace of
    // summaries until the rows were found and removed.
    //
    // Before the assertions, so a failing assertion still cleans up.
    sqlx::query("DELETE FROM communities WHERE workspace_id = $1")
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();

    assert!(
        summarised,
        "the scheduler never summarised the community; global search would have \
         nothing to answer from"
    );
    // ...AND that the summary is READABLE, which is not the same claim.
    //
    // `summaries_by_similarity` — the path the Ask surface takes whenever it
    // has a question vector — INNER JOINs `community_summary_embeddings`. A
    // summary with no embedding is invisible to it, so a workspace can hold
    // valid, current prose and still be reported as "0 themes read". That was
    // the observed symptom: 7 summaries written, 0 consulted.
    assert!(
        embedded,
        "the summary was written but never embedded, so global search cannot see it"
    );
    assert!(
        summariser.calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "a summary row appeared without the summariser being called"
    );
}

/// With NO summariser configured the pass must be a no-op rather than writing
/// placeholder prose. `community_summaries` is persisted and global search
/// answers from it, so a stub summary is indistinguishable from a real one to
/// whoever reads the answer.
#[tokio::test]
async fn no_summariser_means_no_summaries_are_written() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = format!("sched-nosum-{}", Uuid::new_v4());

    let community: Uuid = sqlx::query_scalar(
        "INSERT INTO communities (workspace_id, level, member_set_hash)
         VALUES ($1, 0, $2) RETURNING id",
    )
    .bind(ws)
    .bind(format!("hash-{}", Uuid::new_v4()))
    .fetch_one(&pool)
    .await
    .unwrap();

    let scheduler =
        Scheduler::start(pool.clone(), Arc::new(StubEmbedder(model.clone())), None, None).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    scheduler.stop().await;

    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM community_summaries WHERE community_id = $1")
            .bind(community)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 0, "a summary was written with no summariser configured");
}

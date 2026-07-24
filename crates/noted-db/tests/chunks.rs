use noted_db::chunks;

async fn setup() -> (noted_db::PgPool, uuid::Uuid, uuid::Uuid) {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('chunks-test') RETURNING id")
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

/// Add a live chunk to a page: the chunk row plus the page_chunks link.
/// NOTE there is no `blocks` involvement — chunk hashes and block hashes are
/// different hash spaces and never join. Liveness comes from page_chunks alone.
async fn live_chunk(pool: &noted_db::PgPool, page: uuid::Uuid, hash: &str, text: &str) {
    chunks::upsert(pool, &[(hash.to_string(), text.to_string(), 10)])
        .await
        .unwrap();
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT content_hash FROM page_chunks WHERE page_id = $1 ORDER BY chunk_index",
    )
    .bind(page)
    .fetch_all(pool)
    .await
    .unwrap();
    let mut all = existing;
    all.push(hash.to_string());
    chunks::set_page_chunks(pool, page, &all).await.unwrap();
}

/// A second page in an EXISTING workspace — for tests that need two pages
/// sharing one workspace scope (as opposed to `setup()`, which always mints a
/// fresh workspace of its own).
async fn second_page(pool: &noted_db::PgPool, ws: uuid::Uuid) -> uuid::Uuid {
    sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, 'p2') RETURNING id")
        .bind(ws)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn pending_returns_hashes_with_no_embedding() {
    let (pool, ws, page) = setup().await;
    let h = format!("hash-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "some text").await;

    let pending = chunks::pending(&pool, "m1", Some(ws), 100).await.unwrap();
    assert!(
        pending.iter().any(|c| c.content_hash == h),
        "un-embedded hash must be pending"
    );

    chunks::store_embedding(&pool, &h, "m1", &vec![0.1f32; 768])
        .await
        .unwrap();

    let after = chunks::pending(&pool, "m1", Some(ws), 100).await.unwrap();
    assert!(
        !after.iter().any(|c| c.content_hash == h),
        "embedded hash must not be pending"
    );
}

/// The dirty set is per-model: embedding with one model must not satisfy another.
#[tokio::test]
async fn pending_is_scoped_to_the_model() {
    let (pool, ws, page) = setup().await;
    let h = format!("hash-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "t").await;
    chunks::store_embedding(&pool, &h, "model-a", &vec![0.1f32; 768])
        .await
        .unwrap();

    let other = chunks::pending(&pool, "model-b", Some(ws), 100)
        .await
        .unwrap();
    assert!(
        other.iter().any(|c| c.content_hash == h),
        "a hash embedded with model-a must still be pending for model-b"
    );
}

/// This test hard-codes the same hash `h` for both pages rather than deriving
/// it by hashing identical text, so it does NOT prove content-addressing
/// (identical text -> identical hash) — hash computation lives in `noted-index`,
/// not here, and content-addressing proper is tested there. What this proves is
/// storage-layer behavior: `ON CONFLICT DO NOTHING` collapses duplicate chunk
/// inserts, and `pending()`'s `DISTINCT` collapses a hash referenced by two pages.
///
/// Both pages live in the SAME workspace (via `second_page`, not a second
/// `setup()`) so a workspace-scoped `pending()` call still sees both
/// `page_chunks` links and genuinely exercises the `DISTINCT` collapse —
/// scoping to just one page's workspace would trivially see only one link.
#[tokio::test]
async fn a_hash_referenced_by_two_pages_is_one_chunk_and_queued_once() {
    let (pool, ws, page_a) = setup().await;
    let page_b = second_page(&pool, ws).await;
    let h = format!("shared-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page_a, &h, "shared").await;
    live_chunk(&pool, page_b, &h, "shared").await;

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks WHERE content_hash = $1")
        .bind(&h)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        n, 1,
        "a hash referenced by two pages must produce exactly one chunk row"
    );

    let pending = chunks::pending(&pool, "m1", Some(ws), 1000)
        .await
        .unwrap();
    let times = pending.iter().filter(|c| c.content_hash == h).count();
    assert_eq!(
        times, 1,
        "a shared hash must appear in the dirty set only once"
    );
}

/// An orphaned chunk (row kept, but no page references it) must NOT be work.
/// Otherwise the queue would embed text no user can see, and progress would
/// never reach 100%.
#[tokio::test]
async fn an_orphaned_chunk_is_not_pending() {
    let (pool, ws, page) = setup().await;
    let h = format!("orphan-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "will be orphaned").await;
    assert!(
        chunks::pending(&pool, "m1", Some(ws), 1000)
            .await
            .unwrap()
            .iter()
            .any(|c| c.content_hash == h)
    );

    // The page is edited away to nothing — the chunk row survives, the link does not.
    chunks::set_page_chunks(&pool, page, &[]).await.unwrap();

    let pending = chunks::pending(&pool, "m1", Some(ws), 1000)
        .await
        .unwrap();
    assert!(
        !pending.iter().any(|c| c.content_hash == h),
        "a chunk no page references must not be queued for embedding"
    );
}

/// Coexistence: re-embedding a chunk under a different model must NOT overwrite
/// the first model's vector. Both rows must exist side by side, so the old
/// model keeps serving search while the new model backfills.
#[tokio::test]
async fn two_models_embeddings_coexist_for_one_chunk() {
    let (pool, ws, page) = setup().await;
    let h = format!("coexist-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "coexist text").await;

    chunks::store_embedding(&pool, &h, "model-a", &vec![0.1f32; 768])
        .await
        .unwrap();
    chunks::store_embedding(&pool, &h, "model-b", &vec![0.2f32; 768])
        .await
        .unwrap();

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM embeddings WHERE content_hash = $1")
        .bind(&h)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 2, "both models' embeddings must coexist for one chunk");

    let pending_a = chunks::pending(&pool, "model-a", Some(ws), 1000)
        .await
        .unwrap();
    assert!(
        !pending_a.iter().any(|c| c.content_hash == h),
        "model-a is already embedded and must not be pending"
    );

    let pending_c = chunks::pending(&pool, "model-c", Some(ws), 1000)
        .await
        .unwrap();
    assert!(
        pending_c.iter().any(|c| c.content_hash == h),
        "a third model with no embedding yet must be pending"
    );
}

/// `progress()` must be scopable per workspace so a user-facing "N% indexed"
/// figure never leaks another tenant's backfill volume. This also sidesteps
/// the cross-test-binary flake the unscoped `None` path has (other test
/// binaries' chunks share the global `page_chunks` table).
#[tokio::test]
async fn progress_can_be_scoped_to_one_workspace() {
    let (pool, ws_a, page_a) = setup().await;
    let (_, _ws_b, page_b) = setup().await;
    let model = format!("scope-model-{}", uuid::Uuid::new_v4());

    let h_a = format!("scope-a-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page_a, &h_a, "workspace a text").await;
    let h_b = format!("scope-b-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page_b, &h_b, "workspace b text").await;

    let (_, total_a) = chunks::progress(&pool, &model, Some(ws_a)).await.unwrap();
    assert_eq!(
        total_a, 1,
        "workspace-scoped progress must count only that workspace's chunks"
    );

    let (_, total_global) = chunks::progress(&pool, &model, None).await.unwrap();
    assert!(
        total_global > total_a,
        "the unscoped total ({total_global}) must exceed the scoped total ({total_a}) — \
         it also counts workspace b's chunk"
    );
}

#[tokio::test]
async fn upsert_is_idempotent() {
    let (pool, _, _) = setup().await;
    let h = format!("idem-{}", uuid::Uuid::new_v4());
    chunks::upsert(&pool, &[(h.clone(), "a".into(), 1)])
        .await
        .unwrap();
    chunks::upsert(&pool, &[(h.clone(), "a".into(), 1)])
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks WHERE content_hash = $1")
        .bind(&h)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

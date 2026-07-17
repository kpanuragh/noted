use noted_db::chunks;

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('chunks-test') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    let page: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
        .bind(ws).fetch_one(&pool).await.unwrap();
    (pool, page)
}

/// Add a live chunk to a page: the chunk row plus the page_chunks link.
/// NOTE there is no `blocks` involvement — chunk hashes and block hashes are
/// different hash spaces and never join. Liveness comes from page_chunks alone.
async fn live_chunk(pool: &noted_db::PgPool, page: uuid::Uuid, hash: &str, text: &str) {
    chunks::upsert(pool, &[(hash.to_string(), text.to_string(), 10)]).await.unwrap();
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT content_hash FROM page_chunks WHERE page_id = $1 ORDER BY chunk_index")
        .bind(page).fetch_all(pool).await.unwrap();
    let mut all = existing;
    all.push(hash.to_string());
    chunks::set_page_chunks(pool, page, &all).await.unwrap();
}

#[tokio::test]
async fn pending_returns_hashes_with_no_embedding() {
    let (pool, page) = setup().await;
    let h = format!("hash-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "some text").await;

    let pending = chunks::pending(&pool, "m1", 100).await.unwrap();
    assert!(pending.iter().any(|c| c.content_hash == h), "un-embedded hash must be pending");

    chunks::store_embedding(&pool, &h, "m1", &vec![0.1f32; 768]).await.unwrap();

    let after = chunks::pending(&pool, "m1", 100).await.unwrap();
    assert!(!after.iter().any(|c| c.content_hash == h), "embedded hash must not be pending");
}

/// The dirty set is per-model: embedding with one model must not satisfy another.
#[tokio::test]
async fn pending_is_scoped_to_the_model() {
    let (pool, page) = setup().await;
    let h = format!("hash-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "t").await;
    chunks::store_embedding(&pool, &h, "model-a", &vec![0.1f32; 768]).await.unwrap();

    let other = chunks::pending(&pool, "model-b", 100).await.unwrap();
    assert!(
        other.iter().any(|c| c.content_hash == h),
        "a hash embedded with model-a must still be pending for model-b"
    );
}

/// Content addressing: the same text on two pages is ONE chunk, ONE embedding.
#[tokio::test]
async fn identical_text_on_two_pages_is_one_chunk() {
    let (pool, page_a) = setup().await;
    let (_, page_b) = setup().await;
    let h = format!("shared-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page_a, &h, "shared").await;
    live_chunk(&pool, page_b, &h, "shared").await;

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks WHERE content_hash = $1")
        .bind(&h).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1, "identical text on two pages must produce exactly one chunk row");

    let pending = chunks::pending(&pool, "m1", 1000).await.unwrap();
    let times = pending.iter().filter(|c| c.content_hash == h).count();
    assert_eq!(times, 1, "a shared hash must appear in the dirty set only once");
}

/// An orphaned chunk (row kept, but no page references it) must NOT be work.
/// Otherwise the queue would embed text no user can see, and progress would
/// never reach 100%.
#[tokio::test]
async fn an_orphaned_chunk_is_not_pending() {
    let (pool, page) = setup().await;
    let h = format!("orphan-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "will be orphaned").await;
    assert!(chunks::pending(&pool, "m1", 1000).await.unwrap().iter().any(|c| c.content_hash == h));

    // The page is edited away to nothing — the chunk row survives, the link does not.
    chunks::set_page_chunks(&pool, page, &[]).await.unwrap();

    let pending = chunks::pending(&pool, "m1", 1000).await.unwrap();
    assert!(
        !pending.iter().any(|c| c.content_hash == h),
        "a chunk no page references must not be queued for embedding"
    );
}

#[tokio::test]
async fn upsert_is_idempotent() {
    let (pool, _) = setup().await;
    let h = format!("idem-{}", uuid::Uuid::new_v4());
    chunks::upsert(&pool, &[(h.clone(), "a".into(), 1)]).await.unwrap();
    chunks::upsert(&pool, &[(h.clone(), "a".into(), 1)]).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks WHERE content_hash = $1")
        .bind(&h).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
}

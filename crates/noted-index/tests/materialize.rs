use noted_index::materialize::rechunk_page;

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('mat-test') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    let page: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .unwrap();
    (pool, page)
}

async fn put_block(pool: &noted_db::PgPool, page: uuid::Uuid, idx: i32, text: &str) {
    sqlx::query(
        "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
         VALUES ($1, $2, 'paragraph', $3, md5($3))
         ON CONFLICT (page_id, block_index) DO UPDATE
           SET text = EXCLUDED.text, content_hash = EXCLUDED.content_hash",
    )
    .bind(page)
    .bind(idx)
    .bind(text)
    .execute(pool)
    .await
    .unwrap();
}

fn long(words: usize) -> String {
    std::iter::repeat("word")
        .take(words)
        .collect::<Vec<_>>()
        .join(" ")
}

#[tokio::test]
async fn rechunk_writes_chunks_for_a_page() {
    let (pool, page) = setup().await;
    put_block(&pool, page, 0, &long(100)).await;

    let n = rechunk_page(&pool, page).await.unwrap();
    assert!(n >= 1, "rechunk must produce at least one chunk");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks WHERE text LIKE 'word%'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(count >= 1);
}

#[tokio::test]
async fn rechunk_is_idempotent() {
    let (pool, page) = setup().await;
    put_block(&pool, page, 0, &long(100)).await;

    rechunk_page(&pool, page).await.unwrap();
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks")
        .fetch_one(&pool)
        .await
        .unwrap();
    let links_before: Vec<(i32, String)> = sqlx::query_as(
        "SELECT chunk_index, content_hash FROM page_chunks WHERE page_id = $1 ORDER BY chunk_index",
    )
    .bind(page)
    .fetch_all(&pool)
    .await
    .unwrap();

    rechunk_page(&pool, page).await.unwrap();
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks")
        .fetch_one(&pool)
        .await
        .unwrap();
    let links_after: Vec<(i32, String)> = sqlx::query_as(
        "SELECT chunk_index, content_hash FROM page_chunks WHERE page_id = $1 ORDER BY chunk_index",
    )
    .bind(page)
    .fetch_all(&pool)
    .await
    .unwrap();

    assert_eq!(
        before, after,
        "rechunking unchanged blocks must add no rows"
    );
    assert!(before > 0, "the page must actually have produced chunks");
    assert_eq!(
        links_before, links_after,
        "rechunking unchanged blocks must leave page_chunks links (hashes and order) unchanged"
    );
}

#[tokio::test]
async fn a_page_with_no_blocks_produces_no_chunks() {
    let (pool, page) = setup().await;
    assert_eq!(rechunk_page(&pool, page).await.unwrap(), 0);
}

#[tokio::test]
async fn rechunk_writes_page_chunk_links() {
    let (pool, page) = setup().await;
    put_block(&pool, page, 0, &long(100)).await;

    rechunk_page(&pool, page).await.unwrap();

    let links: Vec<(i32, String)> = sqlx::query_as(
        "SELECT chunk_index, content_hash FROM page_chunks WHERE page_id = $1 ORDER BY chunk_index",
    )
    .bind(page)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        !links.is_empty(),
        "rechunk must write page_chunks links, not just chunk rows"
    );

    // Every linked content_hash must actually resolve to a chunk row (the FK
    // guarantees this, but assert the join returns rows).
    let joined: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM page_chunks pc JOIN chunks c ON c.content_hash = pc.content_hash
         WHERE pc.page_id = $1",
    )
    .bind(page)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        joined as usize,
        links.len(),
        "every page_chunks link must join to a chunks row"
    );

    let indices: Vec<i32> = links.iter().map(|(idx, _)| *idx).collect();
    let expected: Vec<i32> = (0..indices.len() as i32).collect();
    assert_eq!(
        indices, expected,
        "chunk_index must start at 0 and be contiguous"
    );
}

/// ISOLATION: the queue is scoped to THIS test's workspace.
///
/// It used to pass `None`, which drains the whole instance — and `pending` takes
/// a `LIMIT`, so on a shared dev database the assertion silently became "is this
/// chunk among the 100 oldest unembedded chunks in the entire instance". That is
/// a bound every new database test erodes; it went red the first time a fixture
/// pushed the global backlog past 100, in a test file that has nothing to do
/// with chunking. The subject here is `rechunk_page`, not the global backlog, so
/// the queue is scoped — the same "isolate your own DATA SPACE, not just your
/// files" rule the M2a run recorded.
#[tokio::test]
async fn rechunked_chunks_appear_in_the_work_queue() {
    let (pool, page) = setup().await;
    put_block(&pool, page, 0, &format!("distinctiveword {}", long(100))).await;

    rechunk_page(&pool, page).await.unwrap();

    let ws: uuid::Uuid = sqlx::query_scalar("SELECT workspace_id FROM pages WHERE id = $1")
        .bind(page)
        .fetch_one(&pool)
        .await
        .unwrap();
    let pending = noted_db::chunks::pending(&pool, "test-model", Some(ws), 100)
        .await
        .unwrap();
    assert!(
        pending.iter().any(|c| c.text.contains("distinctiveword")),
        "chunks produced by rechunk_page must appear in the work queue: {:?}",
        pending.iter().map(|c| &c.content_hash).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn emptying_a_page_drops_its_stale_links() {
    let (pool, page) = setup().await;
    put_block(&pool, page, 0, &long(100)).await;

    rechunk_page(&pool, page).await.unwrap();
    let links_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM page_chunks WHERE page_id = $1")
            .bind(page)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        links_before > 0,
        "setup must have produced page_chunks links"
    );

    let chunks_before: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks")
        .fetch_one(&pool)
        .await
        .unwrap();

    sqlx::query("DELETE FROM blocks WHERE page_id = $1")
        .bind(page)
        .execute(&pool)
        .await
        .unwrap();
    rechunk_page(&pool, page).await.unwrap();

    let links_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM page_chunks WHERE page_id = $1")
            .bind(page)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        links_after, 0,
        "emptying a page's blocks must drop its stale page_chunks links"
    );

    let chunks_after: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        chunks_after >= chunks_before,
        "chunks rows must not be deleted when a page's links go stale (orphans are kept deliberately)"
    );
}

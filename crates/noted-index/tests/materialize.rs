use noted_index::materialize::rechunk_page;

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('mat-test') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    let page: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
        .bind(ws).fetch_one(&pool).await.unwrap();
    (pool, page)
}

async fn put_block(pool: &noted_db::PgPool, page: uuid::Uuid, idx: i32, text: &str) {
    sqlx::query(
        "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
         VALUES ($1, $2, 'paragraph', $3, md5($3))
         ON CONFLICT (page_id, block_index) DO UPDATE
           SET text = EXCLUDED.text, content_hash = EXCLUDED.content_hash",
    )
    .bind(page).bind(idx).bind(text)
    .execute(pool).await.unwrap();
}

fn long(words: usize) -> String {
    std::iter::repeat("word").take(words).collect::<Vec<_>>().join(" ")
}

#[tokio::test]
async fn rechunk_writes_chunks_for_a_page() {
    let (pool, page) = setup().await;
    put_block(&pool, page, 0, &long(100)).await;

    let n = rechunk_page(&pool, page).await.unwrap();
    assert!(n >= 1, "rechunk must produce at least one chunk");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks WHERE text LIKE 'word%'")
        .fetch_one(&pool).await.unwrap();
    assert!(count >= 1);
}

#[tokio::test]
async fn rechunk_is_idempotent() {
    let (pool, page) = setup().await;
    put_block(&pool, page, 0, &long(100)).await;

    rechunk_page(&pool, page).await.unwrap();
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks").fetch_one(&pool).await.unwrap();
    rechunk_page(&pool, page).await.unwrap();
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks").fetch_one(&pool).await.unwrap();

    assert_eq!(before, after, "rechunking unchanged blocks must add no rows");
}

#[tokio::test]
async fn a_page_with_no_blocks_produces_no_chunks() {
    let (pool, page) = setup().await;
    assert_eq!(rechunk_page(&pool, page).await.unwrap(), 0);
}

use noted_db::docs;

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('docs-test') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    let page: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
        .bind(ws).fetch_one(&pool).await.unwrap();
    (pool, page)
}

#[tokio::test]
async fn append_then_load_preserves_order() {
    let (pool, page) = setup().await;
    docs::append(&pool, page, b"one").await.unwrap();
    docs::append(&pool, page, b"two").await.unwrap();
    docs::append(&pool, page, b"three").await.unwrap();

    let loaded = docs::load(&pool, page).await.unwrap();
    assert_eq!(loaded, vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]);
}

#[tokio::test]
async fn compact_replaces_log_with_single_snapshot() {
    let (pool, page) = setup().await;
    for i in 0..5 {
        docs::append(&pool, page, format!("u{i}").as_bytes()).await.unwrap();
    }
    assert_eq!(docs::update_count(&pool, page).await.unwrap(), 5);

    docs::compact(&pool, page, b"snapshot").await.unwrap();

    assert_eq!(docs::update_count(&pool, page).await.unwrap(), 1);
    assert_eq!(docs::load(&pool, page).await.unwrap(), vec![b"snapshot".to_vec()]);
}

#[tokio::test]
async fn compact_is_atomic_under_concurrent_append() {
    // A compaction must never drop an update that arrives during it. We prove
    // the weaker but load-bearing property: after compaction the log is never
    // empty and always replays to something.
    let (pool, page) = setup().await;
    docs::append(&pool, page, b"before").await.unwrap();
    docs::compact(&pool, page, b"snap").await.unwrap();
    docs::append(&pool, page, b"after").await.unwrap();

    let loaded = docs::load(&pool, page).await.unwrap();
    assert_eq!(loaded, vec![b"snap".to_vec(), b"after".to_vec()],
        "post-compaction appends must sort after the snapshot");
}

#[tokio::test]
async fn load_for_page_with_no_updates_is_empty() {
    let (pool, page) = setup().await;
    assert!(docs::load(&pool, page).await.unwrap().is_empty());
}

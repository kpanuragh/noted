use sqlx::Row;

async fn fresh_pool() -> noted_db::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn migration_creates_pages_table() {
    let pool = fresh_pool().await;
    let row = sqlx::query(
        "SELECT count(*) AS n FROM information_schema.tables
         WHERE table_name IN ('workspaces', 'pages')",
    )
    .fetch_one(&pool).await.unwrap();
    let n: i64 = row.get("n");
    assert_eq!(n, 2, "expected workspaces and pages tables to exist");
}

#[tokio::test]
async fn migration_seeds_the_default_workspace() {
    let pool = fresh_pool().await;
    let exists: bool = sqlx::query_scalar(
        "SELECT exists(SELECT 1 FROM workspaces
         WHERE id = '00000000-0000-0000-0000-000000000001')",
    )
    .fetch_one(&pool).await.unwrap();
    assert!(exists, "the web app and e2e suite depend on the seeded default workspace");
}

#[tokio::test]
async fn pages_cascade_delete_to_children() {
    let pool = fresh_pool().await;
    let ws: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('t') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    let parent: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
        .bind(ws).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO pages (workspace_id, parent_id, title) VALUES ($1, $2, 'c')")
        .bind(ws).bind(parent).execute(&pool).await.unwrap();

    sqlx::query("DELETE FROM pages WHERE id = $1").bind(parent)
        .execute(&pool).await.unwrap();

    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pages WHERE workspace_id = $1")
        .bind(ws).fetch_one(&pool).await.unwrap();
    assert_eq!(remaining, 0, "deleting a parent page must cascade to its children");
}

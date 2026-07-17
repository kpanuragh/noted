use noted_db::search;

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('qf-test') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    (pool, ws)
}

async fn page(pool: &noted_db::PgPool, ws: uuid::Uuid, title: &str) -> uuid::Uuid {
    sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
        .bind(ws).bind(title).fetch_one(pool).await.unwrap()
}

/// The whole point of quick find: an exact title match wins. If this ranks
/// second, the product reads as broken.
#[tokio::test]
async fn an_exact_title_match_ranks_first() {
    let (pool, ws) = setup().await;
    let _ = page(&pool, ws, "Postgres tuning notes from last quarter").await;
    let exact = page(&pool, ws, "Quarterly Report").await;
    let _ = page(&pool, ws, "Quarterly Report follow-up actions").await;

    let hits = search::quick_find(&pool, ws, "Quarterly Report", 10).await.unwrap();
    assert!(!hits.is_empty(), "quick find must return the matching pages");
    assert_eq!(hits[0].page_id, exact, "the exact title match must rank first, got {:?}", hits[0].title);
}

#[tokio::test]
async fn a_prefix_match_is_found() {
    let (pool, ws) = setup().await;
    let p = page(&pool, ws, "Deployment runbook").await;
    let hits = search::quick_find(&pool, ws, "Deploy", 10).await.unwrap();
    assert!(hits.iter().any(|h| h.page_id == p), "a prefix of the title must match");
}

/// Quick find must not leak titles across tenants.
#[tokio::test]
async fn quick_find_is_scoped_to_the_workspace() {
    let (pool, ws_a) = setup().await;
    let (_, ws_b) = setup().await;
    let secret = page(&pool, ws_b, "Acquisition terms").await;

    let hits = search::quick_find(&pool, ws_a, "Acquisition", 10).await.unwrap();
    assert!(
        !hits.iter().any(|h| h.page_id == secret),
        "quick find must never return a page from another workspace"
    );
}

#[tokio::test]
async fn archived_pages_are_not_found() {
    let (pool, ws) = setup().await;
    let p = page(&pool, ws, "Deleted thing").await;
    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(p).execute(&pool).await.unwrap();
    let hits = search::quick_find(&pool, ws, "Deleted", 10).await.unwrap();
    assert!(!hits.iter().any(|h| h.page_id == p), "archived pages must not appear");
}

#[tokio::test]
async fn an_empty_query_returns_nothing_rather_than_everything() {
    let (pool, ws) = setup().await;
    let _ = page(&pool, ws, "Something").await;
    let hits = search::quick_find(&pool, ws, "   ", 10).await.unwrap();
    assert!(hits.is_empty(), "a blank query must not dump the whole workspace");
}

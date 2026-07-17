use noted_db::pages;

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('test') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    (pool, ws)
}

#[tokio::test]
async fn create_then_get_roundtrips() {
    let (pool, ws) = setup().await;
    let created = pages::create(&pool, ws, None, "Hello").await.unwrap();
    let fetched = pages::get(&pool, created.id).await.unwrap().unwrap();
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.title, "Hello");
    assert_eq!(fetched.parent_id, None);
}

#[tokio::test]
async fn children_returns_only_direct_children() {
    let (pool, ws) = setup().await;
    let root = pages::create(&pool, ws, None, "Root").await.unwrap();
    let child = pages::create(&pool, ws, Some(root.id), "Child").await.unwrap();
    let _grandchild = pages::create(&pool, ws, Some(child.id), "Grandchild").await.unwrap();

    let kids = pages::children(&pool, ws, Some(root.id)).await.unwrap();
    assert_eq!(kids.len(), 1, "children() must not recurse into grandchildren");
    assert_eq!(kids[0].id, child.id);
}

#[tokio::test]
async fn get_returns_none_for_unknown_id() {
    let (pool, _ws) = setup().await;
    let missing = pages::get(&pool, uuid::Uuid::new_v4()).await.unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn rename_updates_title_and_bumps_updated_at() {
    let (pool, ws) = setup().await;
    let p = pages::create(&pool, ws, None, "Before").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    pages::rename(&pool, p.id, "After").await.unwrap();
    let after = pages::get(&pool, p.id).await.unwrap().unwrap();
    assert_eq!(after.title, "After");
    assert!(
        after.updated_at > p.updated_at,
        "rename() must bump updated_at: before={}, after={}",
        p.updated_at,
        after.updated_at
    );
}

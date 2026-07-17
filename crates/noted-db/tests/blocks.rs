use noted_crdt::ProjectedBlock;
use noted_db::blocks;

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('blocks-test') RETURNING id")
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

fn block(index: i32, text: &str) -> ProjectedBlock {
    ProjectedBlock {
        index,
        node_type: "paragraph".into(),
        text: text.into(),
        content_hash: format!("hash-{text}"),
    }
}

#[tokio::test]
async fn replace_is_idempotent() {
    let (pool, page) = setup().await;
    let bs = vec![block(0, "one"), block(1, "two")];

    blocks::replace_for_page(&pool, page, &bs).await.unwrap();
    blocks::replace_for_page(&pool, page, &bs).await.unwrap();

    let got = blocks::for_page(&pool, page).await.unwrap();
    assert_eq!(got.len(), 2, "replacing twice must not duplicate rows");
    assert_eq!(got[0].text, "one");
}

#[tokio::test]
async fn replace_removes_deleted_blocks() {
    let (pool, page) = setup().await;
    blocks::replace_for_page(&pool, page, &[block(0, "one"), block(1, "two")])
        .await
        .unwrap();
    blocks::replace_for_page(&pool, page, &[block(0, "one")])
        .await
        .unwrap();

    let got = blocks::for_page(&pool, page).await.unwrap();
    assert_eq!(
        got.len(),
        1,
        "blocks removed from the doc must leave the table"
    );
}

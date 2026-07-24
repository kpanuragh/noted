use noted_crdt::NotedDoc;

mod common;

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    common::ensure_cookie(&pool).await;
    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('idx-test') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    common::join(&pool, ws).await;
    let page: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .unwrap();
    (pool, page)
}

/// A projected page must become chunks without anyone asking. This is the seam
/// between M1a's projection and M1b's pipeline.
#[tokio::test]
async fn projecting_a_page_materialises_chunks() {
    let (pool, page) = setup().await;

    let doc = NotedDoc::new();
    let words = std::iter::repeat("meaningful")
        .take(100)
        .collect::<Vec<_>>()
        .join(" ");
    doc.append_paragraph_for_test(&words);
    noted_db::blocks::replace_for_page(&pool, page, &doc.project())
        .await
        .unwrap();

    let n = noted_index::materialize::rechunk_page(&pool, page)
        .await
        .unwrap();
    assert!(n >= 1, "a projected page must yield chunks");

    let pending = noted_db::chunks::pending(&pool, "any-model", None, 100)
        .await
        .unwrap();
    assert!(
        !pending.is_empty(),
        "fresh chunks must appear in the work queue"
    );
}

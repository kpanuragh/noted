use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

async fn test_app() -> (axum::Router, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('api-test') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    (noted_server::app(noted_server::AppState { pool }), ws)
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_json(res: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn create_page_returns_201_with_body() {
    let (app, ws) = test_app().await;
    let res = app
        .oneshot(post("/api/pages", serde_json::json!({
            "workspace_id": ws, "title": "First"
        })))
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::CREATED);
    let json = body_json(res).await;
    assert_eq!(json["title"], "First");
    assert!(json["id"].is_string());
}

#[tokio::test]
async fn get_unknown_page_returns_404() {
    let (app, _ws) = test_app().await;
    let res = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/pages/{}", uuid::Uuid::new_v4()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

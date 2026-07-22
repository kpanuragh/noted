use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

mod common;

async fn app_and_ws() -> (axum::Router, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    common::ensure_cookie(&pool).await;
    let ws: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('search-api') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO pages (workspace_id, title) VALUES ($1, 'Deployment runbook')")
        .bind(ws).execute(&pool).await.unwrap();
    (noted_server::app(noted_server::AppState::new_for_test(pool)), ws)
}

async fn body_json(res: axum::response::Response) -> serde_json::Value {
    let b = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&b).unwrap()
}

#[tokio::test]
async fn quickfind_returns_matching_pages() {
    let (app, ws) = app_and_ws().await;
    let res = app
        .oneshot(
            Request::builder().header("cookie", common::cookie_header())
                .uri(format!("/api/quickfind?workspace_id={ws}&q=Deploy"))
                .body(Body::empty()).unwrap(),
        )
        .await.unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let json = body_json(res).await;
    let arr = json.as_array().expect("quickfind must return an array");
    assert!(
        arr.iter().any(|h| h["title"] == "Deployment runbook"),
        "quickfind must find the page by a title prefix"
    );
}

#[tokio::test]
async fn quickfind_requires_a_workspace_id() {
    let (app, _) = app_and_ws().await;
    let res = app
        .oneshot(Request::builder().header("cookie", common::cookie_header()).uri("/api/quickfind?q=x").body(Body::empty()).unwrap())
        .await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "a missing workspace_id must 400, not 500");
}

#[tokio::test]
async fn search_embeds_the_query_and_returns_200() {
    let (app, ws) = app_and_ws().await;
    let res = app
        .oneshot(
            Request::builder().header("cookie", common::cookie_header())
                .uri(format!("/api/search?workspace_id={ws}&q=deployment"))
                .body(Body::empty()).unwrap(),
        )
        .await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json = body_json(res).await;
    assert!(json.as_array().is_some(), "search must return an array");
}

#[tokio::test]
async fn search_requires_a_workspace_id() {
    let (app, _) = app_and_ws().await;
    let res = app
        .oneshot(Request::builder().header("cookie", common::cookie_header()).uri("/api/search?q=x").body(Body::empty()).unwrap())
        .await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "a missing workspace_id must 400, not 500");
}

#[tokio::test]
async fn search_with_a_malformed_workspace_id_is_a_400() {
    let (app, _) = app_and_ws().await;
    let res = app
        .oneshot(
            Request::builder().header("cookie", common::cookie_header())
                .uri("/api/search?workspace_id=not-a-uuid&q=x")
                .body(Body::empty()).unwrap(),
        )
        .await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "a malformed workspace_id must 400, not 500"
    );
}

#[tokio::test]
async fn quickfind_with_a_malformed_workspace_id_is_a_400() {
    let (app, _) = app_and_ws().await;
    let res = app
        .oneshot(
            Request::builder().header("cookie", common::cookie_header())
                .uri("/api/quickfind?workspace_id=not-a-uuid&q=x")
                .body(Body::empty()).unwrap(),
        )
        .await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "a malformed workspace_id must 400, not 500"
    );
}

#[tokio::test]
async fn related_for_an_unknown_page_returns_404() {
    let (app, _) = app_and_ws().await;
    let res = app
        .oneshot(
            Request::builder().header("cookie", common::cookie_header())
                .uri(format!("/api/pages/{}/related", uuid::Uuid::new_v4()))
                .body(Body::empty()).unwrap(),
        )
        .await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

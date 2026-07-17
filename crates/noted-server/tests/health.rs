use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

async fn test_app() -> axum::Router {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    noted_server::app(noted_server::AppState { pool })
}

#[tokio::test]
async fn health_reports_ok_and_pgvector_version() {
    let app = test_app().await;
    let res = app
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"], "ok");
    assert!(
        json["pgvector"].as_str().unwrap().starts_with("0.8")
            || json["pgvector"].as_str().unwrap() > "0.8",
        "health must surface a pgvector version meeting the >=0.8 floor, got {}",
        json["pgvector"]
    );
}

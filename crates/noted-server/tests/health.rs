use axum::body::Body;
use axum::http::{Request, StatusCode};
use noted_server::routes::health::{MIN_PGVECTOR, parse_version};
use tower::ServiceExt;

async fn test_app() -> axum::Router {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    noted_server::app(noted_server::AppState::new_for_test(pool))
}

#[tokio::test]
async fn health_reports_ok_and_pgvector_version() {
    let app = test_app().await;
    let res = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"], "ok");
    let version = json["pgvector"].as_str().unwrap();
    let parsed = parse_version(version);
    assert!(
        parsed.is_some_and(|v| v >= MIN_PGVECTOR),
        "health must surface a pgvector version meeting the >=0.8 floor, got {}",
        version
    );
}

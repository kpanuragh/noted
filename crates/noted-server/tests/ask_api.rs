//! M2c Task 5 — the Ask endpoints.
//!
//! Every fixture gets its own workspace; nothing here asserts anything
//! instance-wide.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use noted_server::{app, state::AppState};
use tower::ServiceExt;
use uuid::Uuid;

mod common;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    common::ensure_cookie(&pool).await;
    pool
}

async fn workspace(pool: &noted_db::PgPool) -> Uuid {
    let ws: Uuid = sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('ask-api-test') RETURNING id")

        .fetch_one(pool)

        .await

        .unwrap();

    // Created workspaces are not automatically yours (M4-2).

    common::join(&pool, ws).await;

    ws
}

async fn get(pool: noted_db::PgPool, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = app(AppState::new_for_test(pool))
        .oneshot(Request::builder().header("cookie", common::cookie_header()).uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// An empty workspace answers rather than erroring, and says it has nothing.
///
/// This is the state a brand-new install is in, so it is the first thing a real
/// user will hit.
#[tokio::test]
async fn local_ask_on_an_empty_workspace_is_200_with_no_citations() {
    let pool = pool().await;
    let ws = workspace(&pool).await;

    let (status, body) = get(
        pool,
        &format!("/api/ask/local?workspace_id={ws}&q=anything+at+all"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["citations"].as_array().map(|a| a.len()),
        Some(0),
        "no notes means no evidence, not an error"
    );
    assert!(
        body["answer"].as_str().is_some_and(|s| !s.is_empty()),
        "the emptiness must be stated, got {body:?}"
    );
}

#[tokio::test]
async fn global_ask_reports_how_many_themes_it_could_not_consult() {
    let pool = pool().await;
    let ws = workspace(&pool).await;

    let (status, body) = get(
        pool,
        &format!("/api/ask/global?workspace_id={ws}&q=what+have+I+been+thinking+about"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["skipped_unsummarised"].as_i64(),
        Some(0),
        "an empty workspace has no themes to skip"
    );
    assert_eq!(body["partials"].as_array().map(|a| a.len()), Some(0));
}

/// A blank question is a 400, not a search for "".
///
/// MECHANISM PROTECTED: the `question.is_empty()` guard in both handlers.
/// Removed, the request runs a real (meaningless) search and returns 200.
#[tokio::test]
async fn a_blank_question_is_rejected_on_both_modes() {
    let pool = pool().await;
    let ws = workspace(&pool).await;

    for mode in ["local", "global"] {
        let (status, _) = get(
            pool.clone(),
            &format!("/api/ask/{mode}?workspace_id={ws}&q=+++"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{mode}: whitespace is not a question"
        );
    }
}

/// A malformed `workspace_id` is a 400 from the extractor, on both modes.
#[tokio::test]
async fn a_malformed_workspace_id_is_a_400_on_both_modes() {
    let pool = pool().await;

    for mode in ["local", "global"] {
        let (status, _) = get(
            pool.clone(),
            &format!("/api/ask/{mode}?workspace_id=not-a-uuid&q=hello"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{mode}");
    }
}

/// A missing `workspace_id` is a 400, not a search across every tenant.
#[tokio::test]
async fn ask_requires_a_workspace_id() {
    let pool = pool().await;

    for mode in ["local", "global"] {
        let (status, _) = get(pool.clone(), &format!("/api/ask/{mode}?q=hello")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{mode}");
    }
}

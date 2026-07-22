//! M5-2 — API tokens and per-route scope enforcement.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use noted_server::{app, state::AppState};
use tower::ServiceExt;
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let p = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&p).await.unwrap();
    p
}

/// A user, their workspace, and a token with the given scopes.
async fn token_with(pool: &noted_db::PgPool, scopes: &[&str]) -> (String, Uuid) {
    let email = format!("tok{}@example.com", Uuid::new_v4().simple());
    let (user, _s) = noted_server::auth::sign_up(pool, &email, "api-token-password", "Tok")
        .await.unwrap();
    let ws = noted_db::workspaces::for_user(pool, user.id).await.unwrap()[0].id;

    let (token, hash) = noted_server::auth::new_token();
    let owned: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
    noted_db::api_tokens::create(pool, &hash, user.id, ws, "test", &owned, None)
        .await.unwrap();
    (token, ws)
}

async fn call(pool: noted_db::PgPool, method: &str, uri: &str, token: &str) -> StatusCode {
    app(AppState::new_for_test(pool))
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// **A token reaches only what its scopes allow, and the scope is derived from
/// the route rather than looked up in a list.**
///
/// MECHANISM PROTECTED: the scope check in `token_or_session`. Remove it and
/// every "forbidden" line below turns into a success.
#[tokio::test]
async fn a_token_is_confined_to_its_scopes() {
    let pool = pool().await;
    let (read_only, ws) = token_with(&pool, &["pages:read"]).await;

    // Granted: it can read pages.
    assert_eq!(
        call(pool.clone(), "GET", &format!("/api/pages?workspace_id={ws}"), &read_only).await,
        StatusCode::OK,
        "premise: the granted scope works"
    );

    // Not granted: writing pages, or reading anything else.
    for (method, uri) in [
        ("POST", "/api/pages".to_string()),
        ("GET", format!("/api/search?workspace_id={ws}&q=x")),
        ("GET", format!("/api/ask/local?workspace_id={ws}&q=x")),
        ("GET", format!("/api/workspaces/{ws}/stats")),
    ] {
        assert_eq!(
            call(pool.clone(), method, &uri, &read_only).await,
            StatusCode::FORBIDDEN,
            "{method} {uri} must be refused to a pages:read token"
        );
    }
}

/// Write scope does NOT imply read scope, and vice versa.
///
/// No hierarchy, no wildcards: the rule lives in the token, so someone auditing
/// a leaked token can see exactly what it grants without knowing the checker's
/// implication rules.
#[tokio::test]
async fn write_scope_does_not_imply_read_scope() {
    let pool = pool().await;
    let (writer, ws) = token_with(&pool, &["pages:write"]).await;

    // The claim is "the scope let it through", not any particular downstream
    // status — the empty body here is rejected by the extractor (422), which is
    // proof the request got PAST the scope check rather than being stopped by
    // it.
    let wrote = call(pool.clone(), "POST", "/api/pages", &writer).await;
    assert_ne!(
        wrote,
        StatusCode::FORBIDDEN,
        "a pages:write token must not be blocked from POST /api/pages"
    );
    assert_eq!(
        call(pool, "GET", &format!("/api/pages?workspace_id={ws}"), &writer).await,
        StatusCode::FORBIDDEN,
        "writing must not imply reading"
    );
}

/// **A route nobody has written yet still requires a scope.**
///
/// The derived rule means an unlisted route is not an unguarded one — the
/// failure mode a hand-maintained table has.
#[tokio::test]
async fn an_unknown_api_route_is_not_an_unguarded_one() {
    let pool = pool().await;
    let (token, _ws) = token_with(&pool, &["pages:read"]).await;

    // `/api/exports` does not exist. A token without `exports:read` must be
    // refused BEFORE routing decides it is a 404.
    assert_eq!(
        call(pool, "GET", "/api/exports/2026", &token).await,
        StatusCode::FORBIDDEN
    );
}

/// A forged or revoked token is 401, and revocation takes effect immediately.
#[tokio::test]
async fn a_revoked_token_stops_working_at_once() {
    let pool = pool().await;
    let (token, ws) = token_with(&pool, &["pages:read"]).await;
    let uri = format!("/api/pages?workspace_id={ws}");

    assert_eq!(call(pool.clone(), "GET", &uri, &token).await, StatusCode::OK);

    noted_db::api_tokens::revoke(&pool, &noted_server::auth::hash_token(&token))
        .await.unwrap();

    assert_eq!(call(pool.clone(), "GET", &uri, &token).await, StatusCode::UNAUTHORIZED);
    assert_eq!(
        call(pool, "GET", &uri, "not-a-real-token").await,
        StatusCode::UNAUTHORIZED,
        "and a forgery is indistinguishable"
    );
}

/// An EXPIRED token is refused without any sweeper having run.
#[tokio::test]
async fn an_expired_token_is_refused_without_a_sweeper() {
    let pool = pool().await;
    let email = format!("exp{}@example.com", Uuid::new_v4().simple());
    let (user, _s) = noted_server::auth::sign_up(&pool, &email, "api-token-password", "Exp")
        .await.unwrap();
    let ws = noted_db::workspaces::for_user(&pool, user.id).await.unwrap()[0].id;

    let (token, hash) = noted_server::auth::new_token();
    noted_db::api_tokens::create(
        &pool, &hash, user.id, ws, "expired",
        &["pages:read".to_string()],
        Some(chrono::Utc::now() - chrono::Duration::seconds(1)),
    ).await.unwrap();

    // Sanity: the row exists, so a 401 cannot mean "never created".
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM api_tokens WHERE token_hash = $1")
        .bind(&hash).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);

    assert_eq!(
        call(pool, "GET", &format!("/api/pages?workspace_id={ws}"), &token).await,
        StatusCode::UNAUTHORIZED
    );
}

/// **A token can never reach more than the person who created it.**
///
/// It acts AS its owner, so workspace membership and page ACLs apply to it
/// exactly as they do to a browser session — a token is not a second, weaker
/// permission system.
#[tokio::test]
async fn a_token_cannot_reach_another_users_workspace() {
    let pool = pool().await;
    let (token, _mine) = token_with(&pool, &["pages:read"]).await;
    let (_other_token, theirs) = token_with(&pool, &["pages:read"]).await;

    assert_eq!(
        call(pool, "GET", &format!("/api/pages?workspace_id={theirs}"), &token).await,
        StatusCode::FORBIDDEN,
        "workspace membership still decides, token or not"
    );
}

/// A session still works — tokens are a second way in, not a replacement.
#[tokio::test]
async fn a_browser_session_is_unaffected_by_token_auth() {
    let pool = pool().await;
    let email = format!("sess{}@example.com", Uuid::new_v4().simple());
    let (user, session) = noted_server::auth::sign_up(&pool, &email, "api-token-password", "S")
        .await.unwrap();
    let ws = noted_db::workspaces::for_user(&pool, user.id).await.unwrap()[0].id;

    let status = app(AppState::new_for_test(pool))
        .oneshot(
            Request::builder()
                .uri(format!("/api/pages?workspace_id={ws}"))
                .header("cookie", format!("noted_session={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::OK, "a cookie session needs no scopes");
}

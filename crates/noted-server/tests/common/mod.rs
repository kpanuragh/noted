//! Shared test support.
//!
//! Every `/api` route requires a session (M4-1), so a test that wants to reach
//! one needs a real user and a real token. This creates both through the
//! production path — `auth::sign_up` — rather than inserting rows, so a change
//! to hashing or session issuance breaks the tests that depend on it instead of
//! silently diverging from what the app does.
#![allow(dead_code)]

use axum::body::Body;
use axum::http::Request;
use uuid::Uuid;

/// A live session cookie, in `name=value` form, ready for a `cookie` header.
pub async fn session_cookie(pool: &noted_db::PgPool) -> String {
    let email = format!("t{}@example.com", Uuid::new_v4().simple());
    let (user, token) = noted_server::auth::sign_up(pool, &email, "test-password-long", "Test")
        .await
        .expect("test user creation must succeed");
    let _ = USER_ID.set(user.id);
    format!("noted_session={token}")
}

/// `GET` carrying a session.
pub fn authed_get(uri: &str, cookie: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap()
}

/// `POST`/`PATCH` with a JSON body, carrying a session.
pub fn authed_json(method: &str, uri: &str, cookie: &str, body: String) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("cookie", cookie)
        .body(Body::from(body))
        .unwrap()
}

/// One session shared by every test in a binary.
///
/// Any valid session satisfies the middleware, and these tests are exercising
/// pages/search/dashboard behaviour rather than auth — auth has its own suite
/// (`auth_api.rs`) which builds its sessions explicitly. Sharing one here keeps
/// 34 existing tests from each paying for an Argon2 hash.
static COOKIE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Create the shared session if it does not exist yet. Call once per test, in
/// whatever the file's app-construction helper is.
pub async fn ensure_cookie(pool: &noted_db::PgPool) {
    if COOKIE.get().is_some() {
        return;
    }
    let c = session_cookie(pool).await;
    // A concurrent caller may win the race; both then read the winner's value,
    // and either session is equally valid.
    let _ = COOKIE.set(c);
}

/// The shared session, for a `cookie` header.
pub fn cookie_header() -> &'static str {
    COOKIE.get().map(String::as_str).unwrap_or("")
}

/// The user behind [`session_cookie`], so a test can grant them membership.
static USER_ID: std::sync::OnceLock<Uuid> = std::sync::OnceLock::new();

/// Make the shared test user a member of `workspace_id`.
///
/// Existing tests create their fixture workspaces with a raw INSERT, which
/// (correctly, since M4-2) leaves the caller with no access to them. Rather than
/// weakening the membership check for tests, they now join the workspace they
/// just made — which is what a real user creating a workspace does anyway.
pub async fn join(pool: &noted_db::PgPool, workspace_id: Uuid) {
    ensure_cookie(pool).await;
    let user_id = *USER_ID.get().expect("ensure_cookie sets this");
    noted_db::workspaces::add_member(pool, workspace_id, user_id, "owner")
        .await
        .expect("granting test membership must succeed");
}

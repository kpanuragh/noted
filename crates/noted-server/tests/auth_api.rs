//! M4-1 — authentication.
//!
//! Every fixture uses a unique email; the suite shares a database.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use noted_server::{app, state::AppState};
use tower::ServiceExt;
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

fn email() -> String {
    format!("u{}@example.com", Uuid::new_v4().simple())
}

const GOOD_PASSWORD: &str = "correct-horse-battery-staple";

async fn send(pool: noted_db::PgPool, req: Request<Body>) -> (StatusCode, Vec<String>, String) {
    let res = app(AppState::new_for_test(pool)).oneshot(req).await.unwrap();
    let status = res.status();
    let cookies: Vec<String> = res
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_string))
        .collect();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (status, cookies, String::from_utf8_lossy(&bytes).to_string())
}

fn post(uri: &str, json: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(json.to_string()))
        .unwrap()
}

fn get_with(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().uri(uri);
    if let Some(c) = cookie {
        b = b.header("cookie", c);
    }
    b.body(Body::empty()).unwrap()
}

/// Sign up and return the session cookie in `name=value` form.
async fn signed_up(pool: &noted_db::PgPool) -> String {
    let (status, cookies, _) = send(
        pool.clone(),
        post(
            "/api/auth/signup",
            serde_json::json!({"email": email(), "password": GOOD_PASSWORD}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let raw = cookies.first().expect("signup must set a session cookie");
    raw.split(';').next().unwrap().to_string()
}

// ------------------------------------------------------------ the guarantee --

/// **The headline property: every `/api` path requires a session.**
///
/// The list below deliberately includes paths that DO NOT EXIST. That is the
/// point: the middleware is attached to a nested router covering all of `/api`,
/// so it runs before routing resolves a path. A route added tomorrow is
/// therefore protected without anyone remembering to add it here — and this
/// test proves the property structurally rather than by enumerating a list that
/// would rot the moment it fell out of date.
///
/// It also means an unauthenticated caller cannot distinguish a real route from
/// a fictional one. Both are 401, never 404.
///
/// MECHANISM PROTECTED: the `.layer(from_fn_with_state(.., require_session))` on
/// the protected router in `lib.rs`. Remove it and every line here fails.
#[tokio::test]
async fn every_api_path_requires_a_session_including_ones_that_do_not_exist() {
    let pool = pool().await;
    let ws = Uuid::nil();

    let paths = [
        "/api/me",
        "/api/pages",
        &format!("/api/pages?workspace_id={ws}"),
        "/api/pages/recent",
        &format!("/api/pages/{ws}"),
        &format!("/api/pages/{ws}/related"),
        "/api/quickfind?q=x",
        "/api/search?q=x",
        "/api/ask/local?q=x",
        "/api/ask/global?q=x",
        &format!("/api/workspaces/{ws}/stats"),
        // Not real routes. Still 401 — the guarantee is about the prefix, not
        // about a list of known paths.
        "/api/a-route-nobody-has-written-yet",
        "/api/pages/recent/../../secret",
    ];

    for path in paths {
        let (status, _, _) = send(pool.clone(), get_with(path, None)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "unauthenticated {path} must be 401"
        );
    }
}

/// The sync WebSocket is protected too. An unauthenticated upgrade would stream
/// a page's entire content, which is a worse leak than any REST route.
#[tokio::test]
async fn the_sync_socket_requires_a_session() {
    let pool = pool().await;
    let (status, _, _) = send(
        pool,
        get_with(&format!("/sync/{}", Uuid::new_v4()), None),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// `/health` must NOT require credentials — a liveness probe has none.
#[tokio::test]
async fn health_stays_public() {
    let pool = pool().await;
    let (status, _, _) = send(pool, get_with("/health", None)).await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
}

/// A real session gets through.
#[tokio::test]
async fn a_signed_up_user_can_reach_a_protected_route() {
    let pool = pool().await;
    let cookie = signed_up(&pool).await;

    let (status, _, body) = send(pool, get_with("/api/me", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("@example.com"));
}

/// A garbage or expired token is rejected, and is indistinguishable from an
/// unknown one.
#[tokio::test]
async fn a_forged_token_is_rejected() {
    let pool = pool().await;
    let (status, _, _) = send(
        pool,
        get_with("/api/me", Some("noted_session=deadbeefdeadbeef")),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// An EXPIRED session is rejected — and this is the test that proves expiry is
/// enforced by the lookup query rather than by a sweeper that might not have run.
///
/// MECHANISM PROTECTED: `expires_at > now()` in `users::session_user`. Remove
/// that predicate and this passes a token that should be dead.
#[tokio::test]
async fn an_expired_session_is_rejected_without_any_sweeper_having_run() {
    let pool = pool().await;
    let user = noted_db::users::create(&pool, &email(), "x", "Expired")
        .await
        .unwrap();

    let (token, token_hash) = noted_server::auth::new_token();
    noted_db::users::create_session(
        &pool,
        &token_hash,
        user.id,
        chrono::Utc::now() - chrono::Duration::seconds(1),
    )
    .await
    .unwrap();

    // Sanity: the row IS present. Without this the test could pass because the
    // session was never created at all.
    let present: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sessions WHERE token_hash = $1")
            .bind(&token_hash)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(present, 1, "the expired session row must exist");

    let (status, _, _) = send(
        pool,
        get_with("/api/me", Some(&format!("noted_session={token}"))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// -------------------------------------------------------------- credentials --

#[tokio::test]
async fn sign_in_works_and_is_case_insensitive_on_email() {
    let pool = pool().await;
    let addr = email();

    let (status, _, _) = send(
        pool.clone(),
        post(
            "/api/auth/signup",
            serde_json::json!({"email": &addr, "password": GOOD_PASSWORD}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, cookies, _) = send(
        pool,
        post(
            "/api/auth/signin",
            serde_json::json!({"email": addr.to_uppercase(), "password": GOOD_PASSWORD}),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "uppercasing an email must not create a second, unreachable account"
    );
    assert!(!cookies.is_empty(), "sign-in must set a session cookie");
}

#[tokio::test]
async fn a_wrong_password_is_401_and_an_unknown_email_is_indistinguishable() {
    let pool = pool().await;
    let addr = email();
    send(
        pool.clone(),
        post(
            "/api/auth/signup",
            serde_json::json!({"email": &addr, "password": GOOD_PASSWORD}),
        ),
    )
    .await;

    let (wrong_pw, _, wrong_pw_body) = send(
        pool.clone(),
        post(
            "/api/auth/signin",
            serde_json::json!({"email": &addr, "password": "not-the-password"}),
        ),
    )
    .await;
    let (unknown, _, unknown_body) = send(
        pool,
        post(
            "/api/auth/signin",
            serde_json::json!({"email": email(), "password": GOOD_PASSWORD}),
        ),
    )
    .await;

    assert_eq!(wrong_pw, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown, StatusCode::UNAUTHORIZED);
    assert_eq!(
        wrong_pw_body, unknown_body,
        "a wrong password and an unknown account must be indistinguishable, or sign-in is an \
         account-enumeration oracle"
    );
}

#[tokio::test]
async fn a_duplicate_email_is_a_conflict_not_a_second_account() {
    let pool = pool().await;
    let addr = email();
    let body = serde_json::json!({"email": &addr, "password": GOOD_PASSWORD});

    let (first, _, _) = send(pool.clone(), post("/api/auth/signup", body.clone())).await;
    let (second, _, _) = send(pool.clone(), post("/api/auth/signup", body)).await;

    assert_eq!(first, StatusCode::CREATED);
    assert_eq!(second, StatusCode::CONFLICT);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE lower(email) = lower($1)")
        .bind(&addr)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn a_short_password_is_refused() {
    let pool = pool().await;
    let (status, _, _) = send(
        pool,
        post(
            "/api/auth/signup",
            serde_json::json!({"email": email(), "password": "short"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ------------------------------------------------------------------ secrets --

/// No endpoint may ever emit a password hash.
///
/// MECHANISM PROTECTED: `User` having no `password_hash` field at all. The type
/// is the guard — adding one would make this fail, which is the point.
#[tokio::test]
async fn no_response_ever_contains_a_password_hash() {
    let pool = pool().await;
    let addr = email();

    let (_, _, signup_body) = send(
        pool.clone(),
        post(
            "/api/auth/signup",
            serde_json::json!({"email": &addr, "password": GOOD_PASSWORD}),
        ),
    )
    .await;
    let (_, _, signin_body) = send(
        pool.clone(),
        post(
            "/api/auth/signin",
            serde_json::json!({"email": &addr, "password": GOOD_PASSWORD}),
        ),
    )
    .await;
    let cookie = signed_up(&pool).await;
    let (_, _, me_body) = send(pool, get_with("/api/me", Some(&cookie))).await;

    for (name, body) in [
        ("signup", signup_body),
        ("signin", signin_body),
        ("me", me_body),
    ] {
        assert!(
            !body.contains("password") && !body.contains("$argon2"),
            "{name} response leaked a credential: {body}"
        );
    }
}

/// The session cookie is httpOnly, SameSite, scoped to the whole site, and
/// expires.
#[tokio::test]
async fn the_session_cookie_is_hardened() {
    let pool = pool().await;
    let (_, cookies, _) = send(
        pool,
        post(
            "/api/auth/signup",
            serde_json::json!({"email": email(), "password": GOOD_PASSWORD}),
        ),
    )
    .await;

    let cookie = cookies.first().expect("a cookie must be set");
    assert!(cookie.contains("HttpOnly"), "script must not read it: {cookie}");
    assert!(cookie.contains("SameSite"), "must not ride cross-site: {cookie}");
    assert!(cookie.contains("Path=/"), "the sync socket needs it: {cookie}");
    assert!(cookie.contains("Max-Age="), "must expire: {cookie}");
}

/// Signing out kills the session immediately, and works even when the session
/// is already gone.
#[tokio::test]
async fn signing_out_revokes_the_session_and_is_idempotent() {
    let pool = pool().await;
    let cookie = signed_up(&pool).await;

    // Sanity: it works before we revoke it.
    let (before, _, _) = send(pool.clone(), get_with("/api/me", Some(&cookie))).await;
    assert_eq!(before, StatusCode::OK);

    let out = Request::builder()
        .method("POST")
        .uri("/api/auth/signout")
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(pool.clone(), out).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (after, _, _) = send(pool.clone(), get_with("/api/me", Some(&cookie))).await;
    assert_eq!(after, StatusCode::UNAUTHORIZED, "the token must be dead");

    // Again, with the now-dead cookie: still 204, never 401. A user holding a
    // stale session must be able to sign out rather than being stuck.
    let again = Request::builder()
        .method("POST")
        .uri("/api/auth/signout")
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(pool, again).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

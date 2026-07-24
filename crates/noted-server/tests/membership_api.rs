//! M4-2 — workspace membership.
//!
//! Authentication proved you are *someone*. These tests prove that being
//! someone is not enough: naming a workspace id you do not belong to must fail,
//! on every surface, including the ones addressed by page id rather than by
//! workspace id.
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

/// A signed-up user: their cookie, and the workspace signup made for them.
async fn user(pool: &noted_db::PgPool) -> (String, Uuid) {
    let email = format!("m{}@example.com", Uuid::new_v4().simple());
    let (u, token) = noted_server::auth::sign_up(pool, &email, "membership-test-pw", "Member")
        .await
        .unwrap();
    let ws = noted_db::workspaces::for_user(pool, u.id).await.unwrap();
    assert_eq!(
        ws.len(),
        1,
        "signup must leave the account with exactly one usable workspace"
    );
    (format!("noted_session={token}"), ws[0].id)
}

async fn status(pool: noted_db::PgPool, uri: &str, cookie: &str) -> StatusCode {
    app(AppState::new_for_test(pool))
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// **The headline property: another member's workspace id is refused on every
/// read surface.**
///
/// One assertion per surface rather than a single representative one, because
/// this is the defect class this codebase has produced most — a query that
/// forgot its workspace filter. A surface added later without the
/// `MemberWorkspace` extractor cannot compile with a raw `workspace_id`, which
/// is the structural half; this is the behavioural half.
///
/// MECHANISM PROTECTED: the `check()` call inside the `MemberWorkspace` /
/// `MemberWorkspacePath` extractors. Make it return `Ok(())` unconditionally and
/// every line here fails.
#[tokio::test]
async fn another_users_workspace_is_forbidden_on_every_read_surface() {
    let pool = pool().await;
    let (alice, alice_ws) = user(&pool).await;
    let (_bob, bob_ws) = user(&pool).await;

    // Sanity: Alice's OWN workspace works on the same surfaces. Without this the
    // test could pass because everything 403s for everyone.
    for uri in [
        format!("/api/pages?workspace_id={alice_ws}"),
        format!("/api/pages/recent?workspace_id={alice_ws}"),
        format!("/api/quickfind?workspace_id={alice_ws}&q=x"),
        format!("/api/search?workspace_id={alice_ws}&q=x"),
        format!("/api/ask/local?workspace_id={alice_ws}&q=x"),
        format!("/api/ask/global?workspace_id={alice_ws}&q=x"),
        format!("/api/workspaces/{alice_ws}/stats"),
    ] {
        assert_eq!(
            status(pool.clone(), &uri, &alice).await,
            StatusCode::OK,
            "alice must reach her own workspace: {uri}"
        );
    }

    // And Bob's is refused on all of them.
    for uri in [
        format!("/api/pages?workspace_id={bob_ws}"),
        format!("/api/pages/recent?workspace_id={bob_ws}"),
        format!("/api/quickfind?workspace_id={bob_ws}&q=x"),
        format!("/api/search?workspace_id={bob_ws}&q=x"),
        format!("/api/ask/local?workspace_id={bob_ws}&q=x"),
        format!("/api/ask/global?workspace_id={bob_ws}&q=x"),
        format!("/api/workspaces/{bob_ws}/stats"),
    ] {
        assert_eq!(
            status(pool.clone(), &uri, &alice).await,
            StatusCode::FORBIDDEN,
            "alice must NOT reach bob's workspace: {uri}"
        );
    }
}

/// A page-addressed route leaks nothing either — and answers 404, not 403.
///
/// A non-member must not learn that a page id names something real. 403 says
/// "that exists, just not for you"; 404 says nothing at all.
#[tokio::test]
async fn another_users_page_is_not_found_rather_than_forbidden() {
    let pool = pool().await;
    let (alice, _) = user(&pool).await;
    let (bob, bob_ws) = user(&pool).await;

    // Bob makes a page in his own workspace, through the API.
    let created = app(AppState::new_for_test(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/pages")
                .header("content-type", "application/json")
                .header("cookie", &bob)
                .body(Body::from(
                    serde_json::json!({"workspace_id": bob_ws, "title": "Bob's secret"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(created.into_body(), 1 << 20)
        .await
        .unwrap();
    let page: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let page_id = page["id"].as_str().unwrap();

    // Bob can read it; Alice cannot, and cannot tell it exists.
    assert_eq!(
        status(pool.clone(), &format!("/api/pages/{page_id}"), &bob).await,
        StatusCode::OK
    );
    for uri in [
        format!("/api/pages/{page_id}"),
        format!("/api/pages/{page_id}/related"),
        format!("/sync/{page_id}"),
    ] {
        assert_eq!(
            status(pool.clone(), &uri, &alice).await,
            StatusCode::NOT_FOUND,
            "alice must not learn bob's page exists: {uri}"
        );
    }
}

/// Page CREATION is the one handler that cannot use the extractor — its
/// workspace id is in the JSON body, and a body extractor must run last. So the
/// check there is an explicit line of code, and this is the test that keeps it
/// honest.
#[tokio::test]
async fn a_member_of_no_workspace_cannot_create_a_page_in_one() {
    let pool = pool().await;
    let (alice, _alice_ws) = user(&pool).await;
    let (_bob, bob_ws) = user(&pool).await;

    let res = app(AppState::new_for_test(pool))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/pages")
                .header("content-type", "application/json")
                .header("cookie", &alice)
                .body(Body::from(
                    serde_json::json!({"workspace_id": bob_ws, "title": "trespass"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

/// `GET /api/workspaces` returns MINE and only mine.
#[tokio::test]
async fn the_workspace_list_shows_only_your_own() {
    let pool = pool().await;
    let (alice, alice_ws) = user(&pool).await;
    let (_bob, bob_ws) = user(&pool).await;

    let res = app(AppState::new_for_test(pool))
        .oneshot(
            Request::builder()
                .uri("/api/workspaces")
                .header("cookie", &alice)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let list: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();

    let ids: Vec<&str> = list.iter().filter_map(|w| w["id"].as_str()).collect();
    assert!(
        ids.contains(&alice_ws.to_string().as_str()),
        "alice's own workspace must be listed"
    );
    assert!(
        !ids.contains(&bob_ws.to_string().as_str()),
        "bob's workspace must not be"
    );
}

/// Being invited grants access — the same surfaces that were 403 become 200,
/// which proves the refusal above was about MEMBERSHIP and not about something
/// incidental to the fixture.
#[tokio::test]
async fn adding_a_member_grants_exactly_the_access_that_was_refused() {
    let pool = pool().await;
    let (alice_cookie, _) = user(&pool).await;
    let (_bob, bob_ws) = user(&pool).await;

    let alice = noted_db::users::session_user(
        &pool,
        &noted_server::auth::hash_token(alice_cookie.trim_start_matches("noted_session=")),
    )
    .await
    .unwrap()
    .unwrap();

    let uri = format!("/api/pages?workspace_id={bob_ws}");
    assert_eq!(
        status(pool.clone(), &uri, &alice_cookie).await,
        StatusCode::FORBIDDEN,
        "premise: refused before the invite"
    );

    noted_db::workspaces::add_member(&pool, bob_ws, alice.id, "member")
        .await
        .unwrap();

    assert_eq!(
        status(pool, &uri, &alice_cookie).await,
        StatusCode::OK,
        "and allowed after it"
    );
}

/// A workspace id that does not exist is refused like any other non-membership,
/// rather than 500ing or leaking that it is unknown.
#[tokio::test]
async fn a_nonexistent_workspace_is_forbidden_not_a_server_error() {
    let pool = pool().await;
    let (alice, _) = user(&pool).await;
    let ghost = Uuid::new_v4();

    assert_eq!(
        status(pool, &format!("/api/pages?workspace_id={ghost}"), &alice).await,
        StatusCode::FORBIDDEN
    );
}

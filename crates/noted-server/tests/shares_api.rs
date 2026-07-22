//! M4-4 — share links.
//!
//! The security question here is BLAST RADIUS: a share token is held by someone
//! with no account, so what it reaches must be exactly what was shared and
//! nothing adjacent.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use noted_server::{app, state::AppState};
use tower::ServiceExt;
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

/// A signed-up user with their own workspace.
async fn owner(pool: &noted_db::PgPool) -> (String, Uuid) {
    let email = format!("sh{}@example.com", Uuid::new_v4().simple());
    let (u, token) = noted_server::auth::sign_up(pool, &email, "share-test-password", "Sharer")
        .await
        .unwrap();
    let ws = noted_db::workspaces::for_user(pool, u.id).await.unwrap();
    (format!("noted_session={token}"), ws[0].id)
}

async fn page(pool: &noted_db::PgPool, ws: Uuid, parent: Option<Uuid>, title: &str, body: &str) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, parent_id, title) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(ws)
    .bind(parent)
    .bind(title)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
         VALUES ($1, 0, 'paragraph', $2, md5($2))",
    )
    .bind(id)
    .bind(body)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn send(pool: noted_db::PgPool, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let res = app(AppState::new_for_test(pool)).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// Create a share link, returning its token.
async fn share(
    pool: &noted_db::PgPool,
    cookie: &str,
    page_id: Uuid,
    descendants: bool,
) -> String {
    let (status, body) = send(
        pool.clone(),
        Request::builder()
            .method("POST")
            .uri(format!("/api/pages/{page_id}/share"))
            .header("content-type", "application/json")
            .header("cookie", cookie)
            .body(Body::from(
                serde_json::json!({"include_descendants": descendants}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "share creation failed: {body}");
    body["token"].as_str().unwrap().to_string()
}

async fn read_shared(pool: noted_db::PgPool, token: &str) -> (StatusCode, serde_json::Value) {
    send(
        pool,
        Request::builder()
            .uri(format!("/api/shared/{token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

/// **A share link works with NO session** — that is the entire feature.
#[tokio::test]
async fn a_share_link_is_readable_without_an_account() {
    let pool = pool().await;
    let (cookie, ws) = owner(&pool).await;
    let p = page(&pool, ws, None, "Public thing", "the shared body text").await;

    let token = share(&pool, &cookie, p, false).await;
    let (status, body) = read_shared(pool, &token).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["title"], "Public thing");
    assert_eq!(body[0]["blocks"][0]["text"], "the shared body text");
}

/// **The blast radius: one page, not the tree around it.**
///
/// MECHANISM PROTECTED: `shared_page_ids` returning a single id when the link
/// says no descendants. Widen it and the child leaks.
#[tokio::test]
async fn a_link_without_descendants_reaches_exactly_one_page() {
    let pool = pool().await;
    let (cookie, ws) = owner(&pool).await;
    let parent = page(&pool, ws, None, "Parent", "parent body").await;
    let shared_page = page(&pool, ws, Some(parent), "Shared", "shared body").await;
    let child = page(&pool, ws, Some(shared_page), "Child", "child body").await;
    let sibling = page(&pool, ws, Some(parent), "Sibling", "sibling body").await;

    let token = share(&pool, &cookie, shared_page, false).await;
    let (status, body) = read_shared(pool, &token).await;

    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();

    assert_eq!(ids, vec![shared_page.to_string()], "exactly the shared page");
    for (name, id) in [("child", child), ("sibling", sibling), ("parent", parent)] {
        assert!(
            !ids.contains(&id.to_string().as_str()),
            "{name} must not be reachable"
        );
    }
}

/// With descendants, the link reaches DOWN only — never up to a parent or
/// across to a sibling.
#[tokio::test]
async fn a_descendant_link_reaches_down_but_never_up_or_sideways() {
    let pool = pool().await;
    let (cookie, ws) = owner(&pool).await;
    let parent = page(&pool, ws, None, "Parent", "parent body").await;
    let shared_page = page(&pool, ws, Some(parent), "Shared", "shared body").await;
    let child = page(&pool, ws, Some(shared_page), "Child", "child body").await;
    let grandchild = page(&pool, ws, Some(child), "Grandchild", "grandchild body").await;
    let sibling = page(&pool, ws, Some(parent), "Sibling", "sibling body").await;

    let token = share(&pool, &cookie, shared_page, true).await;
    let (_, body) = read_shared(pool, &token).await;
    let ids: Vec<String> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect();

    for (name, id) in [("shared", shared_page), ("child", child), ("grandchild", grandchild)] {
        assert!(ids.contains(&id.to_string()), "{name} must be reachable");
    }
    for (name, id) in [("parent", parent), ("sibling", sibling)] {
        assert!(!ids.contains(&id.to_string()), "{name} must NOT be");
    }
}

/// **A revoked token is dead immediately** — no cache, no grace period.
#[tokio::test]
async fn revoking_a_link_kills_it_at_once() {
    let pool = pool().await;
    let (cookie, ws) = owner(&pool).await;
    let p = page(&pool, ws, None, "Temporary", "body").await;
    let token = share(&pool, &cookie, p, false).await;

    // Premise: it works before revocation.
    let (before, _) = read_shared(pool.clone(), &token).await;
    assert_eq!(before, StatusCode::OK);

    let (status, _) = send(
        pool.clone(),
        Request::builder()
            .method("DELETE")
            .uri(format!("/api/shares/{token}"))
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (after, _) = read_shared(pool, &token).await;
    assert_eq!(after, StatusCode::NOT_FOUND, "the token must be dead");
}

/// An EXPIRED link is dead without any sweeper having run.
///
/// MECHANISM PROTECTED: `expires_at IS NULL OR expires_at > now()` in `resolve`.
#[tokio::test]
async fn an_expired_link_is_dead_without_a_sweeper() {
    let pool = pool().await;
    let (_cookie, ws) = owner(&pool).await;
    let p = page(&pool, ws, None, "Expiring", "body").await;

    let (token, token_hash) = noted_server::auth::new_token();
    let user_id: Uuid = sqlx::query_scalar("SELECT user_id FROM workspace_members WHERE workspace_id = $1")
        .bind(ws)
        .fetch_one(&pool)
        .await
        .unwrap();
    noted_db::shares::create(
        &pool,
        &token_hash,
        p,
        user_id,
        false,
        Some(chrono::Utc::now() - chrono::Duration::seconds(1)),
    )
    .await
    .unwrap();

    // Sanity: the row exists. Without this the 404 could mean "never created".
    let present: i64 = sqlx::query_scalar("SELECT count(*) FROM share_links WHERE token_hash = $1")
        .bind(&token_hash)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(present, 1);

    let (status, _) = read_shared(pool, &token).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Archiving a shared page revokes its links implicitly. A user deleting a page
/// is not thinking about the link they sent last month, but they certainly mean
/// for it to stop working.
#[tokio::test]
async fn archiving_a_shared_page_kills_its_links() {
    let pool = pool().await;
    let (cookie, ws) = owner(&pool).await;
    let p = page(&pool, ws, None, "Doomed", "body").await;
    let token = share(&pool, &cookie, p, false).await;

    let (before, _) = read_shared(pool.clone(), &token).await;
    assert_eq!(before, StatusCode::OK);

    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(p)
        .execute(&pool)
        .await
        .unwrap();

    let (after, _) = read_shared(pool, &token).await;
    assert_eq!(after, StatusCode::NOT_FOUND);
}

/// A forged token is 404 — identical to a revoked or expired one, so a stranger
/// cannot learn that a token was ever real.
#[tokio::test]
async fn a_forged_token_is_indistinguishable_from_a_revoked_one() {
    let pool = pool().await;
    let (status, _) = read_shared(pool, "deadbeefdeadbeefdeadbeef").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// You cannot share a page you cannot read.
///
/// The create route sits behind `MemberPage`, so both non-membership and a
/// page-level denial block it — publishing is strictly more than reading, and
/// must never be reachable by someone who lacks even that.
#[tokio::test]
async fn a_stranger_cannot_create_a_share_link_for_your_page() {
    let pool = pool().await;
    let (_owner_cookie, ws) = owner(&pool).await;
    let (stranger_cookie, _their_ws) = owner(&pool).await;
    let p = page(&pool, ws, None, "Private", "body").await;

    let (status, _) = send(
        pool,
        Request::builder()
            .method("POST")
            .uri(format!("/api/pages/{p}/share"))
            .header("content-type", "application/json")
            .header("cookie", &stranger_cookie)
            .body(Body::from(serde_json::json!({}).to_string()))
            .unwrap(),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a non-member must not be able to publish someone else's page"
    );
}

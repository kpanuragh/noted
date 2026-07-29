use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

mod common;

async fn test_app() -> (axum::Router, uuid::Uuid) {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    common::ensure_cookie(&pool).await;
    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('api-test') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    common::join(&pool, ws).await;
    (noted_server::app(noted_server::AppState::new_for_test(pool)), ws)
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder().header("cookie", common::cookie_header())
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_json(res: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn create_page_returns_201_with_body() {
    let (app, ws) = test_app().await;
    let res = app
        .oneshot(post(
            "/api/pages",
            serde_json::json!({
                "workspace_id": ws, "title": "First"
            }),
        ))
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
            Request::builder().header("cookie", common::cookie_header())
                .uri(format!("/api/pages/{}", uuid::Uuid::new_v4()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_existing_page_returns_200_with_body() {
    let (app, ws) = test_app().await;
    let create_res = app
        .clone()
        .oneshot(post(
            "/api/pages",
            serde_json::json!({
                "workspace_id": ws, "title": "Fetchable"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(create_res.status(), StatusCode::CREATED);
    let created = body_json(create_res).await;
    let id = created["id"].as_str().unwrap();

    let res = app
        .oneshot(
            Request::builder().header("cookie", common::cookie_header())
                .uri(format!("/api/pages/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json = body_json(res).await;
    assert_eq!(json["id"], created["id"]);
    assert_eq!(json["title"], "Fetchable");
}

#[tokio::test]
async fn list_returns_only_direct_children() {
    let (app, ws) = test_app().await;

    let root_res = app
        .clone()
        .oneshot(post(
            "/api/pages",
            serde_json::json!({
                "workspace_id": ws, "title": "Root"
            }),
        ))
        .await
        .unwrap();
    let root = body_json(root_res).await;
    let root_id = root["id"].as_str().unwrap();

    let child_res = app
        .clone()
        .oneshot(post(
            "/api/pages",
            serde_json::json!({
                "workspace_id": ws, "parent_id": root_id, "title": "Child"
            }),
        ))
        .await
        .unwrap();
    let child = body_json(child_res).await;
    let child_id = child["id"].as_str().unwrap();

    let _grandchild_res = app
        .clone()
        .oneshot(post(
            "/api/pages",
            serde_json::json!({
                "workspace_id": ws, "parent_id": child_id, "title": "Grandchild"
            }),
        ))
        .await
        .unwrap();

    let res = app
        .oneshot(
            Request::builder().header("cookie", common::cookie_header())
                .uri(format!("/api/pages?workspace_id={ws}&parent_id={root_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json = body_json(res).await;
    let arr = json.as_array().unwrap();
    assert_eq!(
        arr.len(),
        1,
        "expected exactly one direct child, got {json:?}"
    );
    assert_eq!(arr[0]["id"], child["id"]);
}

#[tokio::test]
async fn rename_missing_page_returns_404() {
    let (app, _ws) = test_app().await;
    let res = app
        .oneshot(
            Request::builder().header("cookie", common::cookie_header())
                .method("PATCH")
                .uri(format!("/api/pages/{}", uuid::Uuid::new_v4()))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"title": "x"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

/// Creating a page in an unknown workspace is FORBIDDEN, not a bad request.
///
/// It used to be 400: the insert hit a foreign-key violation and the handler
/// mapped that to `InvalidReference`. Membership (M4-2) now answers earlier and
/// better — you are not a member of a workspace that does not exist, and the
/// reply is identical to the one for a workspace that exists and is not yours.
/// The old 400 quietly distinguished those two cases.
#[tokio::test]
async fn create_with_unknown_workspace_is_forbidden() {
    let (app, _ws) = test_app().await;
    let res = app
        .oneshot(post(
            "/api/pages",
            serde_json::json!({
                "workspace_id": uuid::Uuid::new_v4(), "title": "Orphan"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

fn delete_req(uri: &str) -> Request<Body> {
    Request::builder()
        .header("cookie", common::cookie_header())
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn get_req(uri: &str) -> Request<Body> {
    Request::builder()
        .header("cookie", common::cookie_header())
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn deleting_a_page_makes_it_gone() {
    let (app, ws) = test_app().await;
    let created = app
        .clone()
        .oneshot(post("/api/pages", serde_json::json!({ "workspace_id": ws, "title": "Bye" })))
        .await
        .unwrap();
    let id = body_json(created).await["id"].as_str().unwrap().to_string();

    let res = app.clone().oneshot(delete_req(&format!("/api/pages/{id}"))).await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    // The page view relies on this: a deleted note must 404 so the client can
    // say "this note no longer exists" rather than load forever.
    let after = app.oneshot(get_req(&format!("/api/pages/{id}"))).await.unwrap();
    assert_eq!(after.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_a_page_twice_reports_404_rather_than_succeeding_again() {
    let (app, ws) = test_app().await;
    let created = app
        .clone()
        .oneshot(post("/api/pages", serde_json::json!({ "workspace_id": ws, "title": "Twice" })))
        .await
        .unwrap();
    let id = body_json(created).await["id"].as_str().unwrap().to_string();

    let first = app.clone().oneshot(delete_req(&format!("/api/pages/{id}"))).await.unwrap();
    assert_eq!(first.status(), StatusCode::NO_CONTENT);
    let second = app.oneshot(delete_req(&format!("/api/pages/{id}"))).await.unwrap();
    assert_eq!(
        second.status(),
        StatusCode::NOT_FOUND,
        "a second delete did work it should not have"
    );
}

/// Deletion is the most destructive thing this API does, so the membership
/// check matters more here than anywhere else: a page id is a bare uuid in a
/// URL, and guessing one must not let a stranger destroy someone's note.
#[tokio::test]
async fn a_non_member_cannot_delete_someone_elses_page() {
    let (app, _ws) = test_app().await;
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    // A workspace this session was never joined to.
    let other: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('not-mine') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    let page = noted_db::pages::create(&pool, other, None, "Theirs").await.unwrap();

    let res = app.oneshot(delete_req(&format!("/api/pages/{}", page.id))).await.unwrap();
    assert_ne!(res.status(), StatusCode::NO_CONTENT, "a non-member deleted a page");
    assert!(noted_db::pages::get(&pool, page.id).await.unwrap().is_some());
}

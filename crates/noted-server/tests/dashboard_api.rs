use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

async fn test_app() -> (axum::Router, noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('dash-test') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    (
        noted_server::app(noted_server::AppState::new_for_test(pool.clone())),
        pool,
        ws,
    )
}

fn get(uri: String) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn body_json(res: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ---------------------------------------------------------------- recent ----

/// `recent` is a STATIC segment sharing a prefix with `/api/pages/{id}`, whose
/// extractor parses a uuid. If the router ever preferred the dynamic route,
/// this endpoint would start returning 400 instead of a list — a failure mode
/// that no test of `pages::recent` itself could ever see.
#[tokio::test]
async fn recent_is_not_shadowed_by_the_page_id_route() {
    let (app, _pool, ws) = test_app().await;
    let res = app
        .oneshot(get(format!("/api/pages/recent?workspace_id={ws}")))
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "GET /api/pages/recent must reach the recent handler, not /api/pages/{{id}}"
    );
}

/// The response must be the SAME `Page` shape the list endpoint returns, so the
/// dashboard can reuse one client-side type.
#[tokio::test]
async fn recent_returns_the_page_shape_ordered_by_edit_time() {
    let (app, pool, ws) = test_app().await;
    let older = noted_db::pages::create(&pool, ws, None, "Older")
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let newer = noted_db::pages::create(&pool, ws, None, "Newer")
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    noted_db::docs::append(&pool, older.id, b"edit")
        .await
        .unwrap();

    let res = app
        .oneshot(get(format!("/api/pages/recent?workspace_id={ws}")))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json = body_json(res).await;
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(
        arr[0]["id"].as_str().unwrap(),
        older.id.to_string(),
        "the most recently EDITED page must come first, got {json:?}"
    );
    assert_eq!(arr[1]["id"].as_str().unwrap(), newer.id.to_string());
    for field in ["id", "workspace_id", "title", "created_at", "updated_at"] {
        assert!(
            arr[0].get(field).is_some(),
            "missing `{field}`: the response must match the list endpoint's Page shape"
        );
    }
}

#[tokio::test]
async fn recent_honours_limit_and_caps_it() {
    let (app, pool, ws) = test_app().await;
    sqlx::query(
        "INSERT INTO pages (workspace_id, title)
         SELECT $1, 'p' || g::text FROM generate_series(1, $2) AS g",
    )
    .bind(ws)
    .bind(noted_db::pages::MAX_RECENT_LIMIT as i32 + 10)
    .execute(&pool)
    .await
    .unwrap();

    let res = app
        .clone()
        .oneshot(get(format!("/api/pages/recent?workspace_id={ws}&limit=3")))
        .await
        .unwrap();
    assert_eq!(body_json(res).await.as_array().unwrap().len(), 3);

    // The default, when the caller asks for no particular number.
    let res = app
        .clone()
        .oneshot(get(format!("/api/pages/recent?workspace_id={ws}")))
        .await
        .unwrap();
    assert_eq!(
        body_json(res).await.as_array().unwrap().len(),
        10,
        "the default limit must be 10"
    );

    // An uncapped limit is a trivial denial of service.
    let res = app
        .oneshot(get(format!(
            "/api/pages/recent?workspace_id={ws}&limit=1000000"
        )))
        .await
        .unwrap();
    let n = body_json(res).await.as_array().unwrap().len() as i64;
    assert!(
        n <= noted_db::pages::MAX_RECENT_LIMIT,
        "the endpoint must cap the limit at {}, returned {n}",
        noted_db::pages::MAX_RECENT_LIMIT
    );
}

#[tokio::test]
async fn recent_without_a_workspace_id_is_400() {
    let (app, _pool, _ws) = test_app().await;
    let res = app.oneshot(get("/api/pages/recent".into())).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn recent_with_a_malformed_workspace_id_is_400() {
    let (app, _pool, _ws) = test_app().await;
    let res = app
        .oneshot(get("/api/pages/recent?workspace_id=not-a-uuid".into()))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

// ----------------------------------------------------------------- stats ----

#[tokio::test]
async fn stats_returns_the_four_counters_as_numbers() {
    let (app, pool, ws) = test_app().await;
    let p = noted_db::pages::create(&pool, ws, None, "One")
        .await
        .unwrap();
    let gone = noted_db::pages::create(&pool, ws, None, "Gone")
        .await
        .unwrap();
    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(gone.id)
        .execute(&pool)
        .await
        .unwrap();

    // The route measures chunks against the AppState embedder's model id, which
    // is "stub" under new_for_test — so seed the embedding under that same id
    // rather than a literal of our own. This is the read/write agreement the
    // search routes already rely on.
    let hash = format!("dash-{}", uuid::Uuid::new_v4());
    noted_db::chunks::upsert(&pool, &[(hash.clone(), "text".into(), 10)])
        .await
        .unwrap();
    noted_db::chunks::set_page_chunks(&pool, p.id, &[hash.clone()])
        .await
        .unwrap();
    noted_db::chunks::store_embedding(&pool, &hash, "stub", &vec![0.1_f32; 768])
        .await
        .unwrap();

    let a = noted_db::graph::resolve_entity(&pool, ws, "alice", Some("PERSON"), None)
        .await
        .unwrap();
    let b = noted_db::graph::resolve_entity(&pool, ws, "bob", Some("PERSON"), None)
        .await
        .unwrap();
    noted_db::graph::replace_chunk_edges(&pool, ws, &hash, "stub", &[(a, b, "knows".into(), 1.0)])
        .await
        .unwrap();

    let res = app
        .oneshot(get(format!("/api/workspaces/{ws}/stats")))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json = body_json(res).await;
    assert_eq!(
        json["pages"], 1,
        "archived pages must not be counted: {json:?}"
    );
    assert_eq!(json["chunks_indexed"], 1);
    assert_eq!(json["entities"], 2);
    assert_eq!(json["edges"], 1);
    for field in ["pages", "chunks_indexed", "entities", "edges"] {
        assert!(
            json[field].is_i64(),
            "`{field}` must serialise as a JSON number, got {:?}",
            json[field]
        );
    }
}

/// Tenancy, through the HTTP surface this time. A second workspace with its own
/// pages, chunks, entities and edges must contribute nothing.
#[tokio::test]
async fn stats_does_not_count_another_workspaces_rows() {
    let (app, pool, ws_a) = test_app().await;
    let ws_b: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('dash-other') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();

    let pb = noted_db::pages::create(&pool, ws_b, None, "Theirs")
        .await
        .unwrap();
    let hash = format!("other-{}", uuid::Uuid::new_v4());
    noted_db::chunks::upsert(&pool, &[(hash.clone(), "their text".into(), 10)])
        .await
        .unwrap();
    noted_db::chunks::set_page_chunks(&pool, pb.id, &[hash.clone()])
        .await
        .unwrap();
    noted_db::chunks::store_embedding(&pool, &hash, "stub", &vec![0.2_f32; 768])
        .await
        .unwrap();
    let x = noted_db::graph::resolve_entity(&pool, ws_b, "xavier", Some("PERSON"), None)
        .await
        .unwrap();
    let y = noted_db::graph::resolve_entity(&pool, ws_b, "yolanda", Some("PERSON"), None)
        .await
        .unwrap();
    noted_db::graph::replace_chunk_edges(
        &pool,
        ws_b,
        &hash,
        "stub",
        &[(x, y, "knows".into(), 1.0)],
    )
    .await
    .unwrap();

    let res = app
        .oneshot(get(format!("/api/workspaces/{ws_a}/stats")))
        .await
        .unwrap();
    let json = body_json(res).await;
    assert_eq!(
        json,
        serde_json::json!({"pages": 0, "chunks_indexed": 0, "entities": 0, "edges": 0}),
        "workspace A is empty; every one of these counters is another tenant's data leaking"
    );
}

#[tokio::test]
async fn stats_for_an_unknown_workspace_is_zeroes_not_an_error() {
    let (app, _pool, _ws) = test_app().await;
    let res = app
        .oneshot(get(format!(
            "/api/workspaces/{}/stats",
            uuid::Uuid::new_v4()
        )))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json = body_json(res).await;
    assert_eq!(json["pages"], 0);
}

#[tokio::test]
async fn stats_with_a_malformed_workspace_id_is_400() {
    let (app, _pool, _ws) = test_app().await;
    let res = app
        .oneshot(get("/api/workspaces/not-a-uuid/stats".into()))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

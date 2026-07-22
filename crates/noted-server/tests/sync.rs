use futures_util::{SinkExt, StreamExt};
use noted_crdt::NotedDoc;
use noted_db::docs;
use noted_server::AppState;
use noted_server::routes::sync::{SyncMsg, encode_msg, parse_msg};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

mod common;

#[test]
fn parses_sync_step1() {
    let msg = parse_msg(&[0, 0, 1, 2, 3]).unwrap();
    match msg {
        SyncMsg::Step1(sv) => assert_eq!(sv, vec![1, 2, 3]),
        other => panic!("expected Step1, got {other:?}"),
    }
}

#[test]
fn parses_update() {
    let msg = parse_msg(&[0, 2, 9, 9]).unwrap();
    match msg {
        SyncMsg::Update(u) => assert_eq!(u, vec![9, 9]),
        other => panic!("expected Update, got {other:?}"),
    }
}

#[test]
fn rejects_unknown_message_type() {
    assert!(
        parse_msg(&[0, 99, 1]).is_none(),
        "unknown subtype must not panic"
    );
    assert!(
        parse_msg(&[7, 0, 1]).is_none(),
        "non-sync message must not panic"
    );
    assert!(parse_msg(&[0]).is_none(), "truncated frame must not panic");
    assert!(parse_msg(&[]).is_none(), "empty frame must not panic");
}

#[test]
fn encode_roundtrips_through_parse() {
    let encoded = encode_msg(&SyncMsg::Step2(vec![4, 5, 6]));
    match parse_msg(&encoded).unwrap() {
        SyncMsg::Step2(d) => assert_eq!(d, vec![4, 5, 6]),
        other => panic!("expected Step2, got {other:?}"),
    }
}

/// The server's reply to a Step1 must carry exactly what the client lacks.
#[test]
fn step2_reply_brings_a_fresh_client_up_to_date() {
    let server = NotedDoc::new();
    server.append_paragraph_for_test("server-side content");

    let client = NotedDoc::new();
    let diff = server.diff(&client.state_vector()).unwrap();
    client.apply_update(&diff).unwrap();

    assert_eq!(client.text_for_test(), "server-side content");
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Serve `app` on an ephemeral port, returning its address.
async fn serve(app: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Connect to the sync socket WITH a session cookie.
///
/// The upgrade is an ordinary HTTP request until the moment it is not, so it
/// carries cookies and the auth middleware sees it like any other route. That
/// is exactly why `/sync/{page_id}` sits inside the protected router: an
/// unauthenticated socket would stream a page's entire content, which is a
/// worse leak than any REST endpoint. `auth_api.rs` asserts the rejection; this
/// helper is the other half — proving a legitimate client can still connect.
async fn connect(addr: std::net::SocketAddr, page_id: uuid::Uuid) -> Ws {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{addr}/sync/{page_id}")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "cookie",
        common::cookie_header().parse().expect("valid cookie header"),
    );
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("failed to connect to sync websocket");
    ws
}

async fn send_update(ws: &mut Ws, update: Vec<u8>) {
    ws.send(WsMessage::Binary(encode_msg(&SyncMsg::Update(update))))
        .await
        .expect("failed to send update frame");
}

/// Read frames until one parses as a `SyncMsg::Update`, or `None` on timeout.
/// Step1/Step2 handshake frames are skipped.
async fn next_update(ws: &mut Ws, within: Duration) -> Option<Vec<u8>> {
    tokio::time::timeout(within, async {
        while let Some(Ok(frame)) = ws.next().await {
            if let WsMessage::Binary(bytes) = frame
                && let Some(SyncMsg::Update(u)) = parse_msg(&bytes)
            {
                return Some(u);
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

/// Poll `blocks` until `want` matches, returning the last value seen. Polling
/// (rather than a fixed sleep) keeps this robust against the projection
/// debounce and CI timing.
async fn await_block_texts(
    pool: &noted_db::PgPool,
    page_id: uuid::Uuid,
    want: &[&str],
) -> Vec<String> {
    let mut seen = Vec::new();
    for _ in 0..100 {
        seen = noted_db::blocks::for_page(pool, page_id)
            .await
            .unwrap()
            .into_iter()
            .map(|b| b.text)
            .collect();
        if seen == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    seen
}

/// Sets up a workspace + page directly via sqlx, mirroring the `setup()`
/// helper in `crates/noted-db/tests/docs.rs`.
async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    common::ensure_cookie(&pool).await;
    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('sync-ws-test') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    let page: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .unwrap();
    (pool, page)
}

/// End-to-end proof that the `/sync/{page_id}` WebSocket handler actually
/// persists client updates to the `doc_updates` log (Postgres, the source of
/// truth). This is the only test in this file that touches `session()` — the
/// unit tests above only exercise `parse_msg`/`encode_msg`/`NotedDoc`.
///
/// This test must FAIL if `session()` stops calling `docs::append`; see the
/// task report for before/after evidence of that failure mode.
#[tokio::test]
async fn websocket_session_persists_an_update() {
    let (pool, page_id) = setup().await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = noted_server::app(AppState::new_for_test(pool.clone()));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Carries the session like `connect()` does — the sync socket is inside the
    // protected router, so an upgrade without a cookie is a 401.
    let mut ws_stream = connect(addr, page_id).await;

    // Real CRDT update produced by the same NotedDoc machinery the server
    // uses, so this is a realistic y-protocols Update payload, not a stub.
    let doc = NotedDoc::new();
    let update = doc.append_paragraph_for_test("hello over the wire");

    let frame = encode_msg(&SyncMsg::Update(update));
    ws_stream
        .send(tokio_tungstenite::tungstenite::Message::Binary(frame))
        .await
        .expect("failed to send update frame");

    // Poll for up to ~2s for the server to persist the update.
    let mut loaded = Vec::new();
    for _ in 0..40 {
        loaded = docs::load(&pool, page_id).await.unwrap();
        if !loaded.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert!(
        !loaded.is_empty(),
        "expected the server to persist at least one update to doc_updates"
    );

    let replayed = NotedDoc::from_updates(&loaded).unwrap();
    assert!(
        replayed.text_for_test().contains("hello over the wire"),
        "replayed doc text was {:?}",
        replayed.text_for_test()
    );
}

/// Two tabs on ONE page must share ONE server-side document.
///
/// Before the page hub existed, every session replayed its own private
/// `NotedDoc` at connect time. Session B therefore never saw session A's edits,
/// and B's `blocks::replace_for_page` DELETEd A's rows and wrote B's stale view
/// — leaving the projection wrong even though the CRDT log was fine.
///
/// This test pins both halves of the fix: B is handed A's update over the
/// broadcast channel, and the resulting projection contains BOTH edits in order
/// rather than one clobbering the other. It must FAIL if sessions go back to
/// owning private documents.
#[tokio::test]
async fn two_sessions_on_one_page_share_a_document() {
    let (pool, page_id) = setup().await;
    let state = AppState::new_for_test(pool.clone());
    let addr = serve(noted_server::app(state.clone())).await;

    // Both tabs connect BEFORE any edit, so the only way B can learn about
    // "alpha" is the live broadcast — not the connect-time Step1 handshake.
    let mut a = connect(addr, page_id).await;
    let mut b = connect(addr, page_id).await;

    // Wait for the server's opening handshake frame to B BEFORE A types.
    //
    // `connect` returns as soon as the websocket upgrade completes, which is
    // not the same instant the server has registered B as a subscriber on the
    // page's hub. If A broadcasts inside that window the update goes to nobody
    // and B waits out the timeout. The server sends Step1 on connect, so
    // receiving any frame proves B's session exists server-side.
    //
    // The race was always here; adding the auth middleware (one session lookup
    // per upgrade) widened the window enough to make it fail about one run in
    // three. Waiting on the handshake closes it properly — raising the timeout
    // would only have made a real bug take longer to fail.
    tokio::time::timeout(Duration::from_secs(5), b.next())
        .await
        .expect("timed out waiting for B's opening handshake frame")
        .expect("B's socket closed before the handshake")
        .expect("B's handshake frame was an error");

    // Tab A types.
    let doc_a = NotedDoc::new();
    let update_a = doc_a.append_paragraph_for_test("alpha");
    send_update(&mut a, update_a.clone()).await;

    // Tab B must receive A's edit over the broadcast.
    let relayed = next_update(&mut b, Duration::from_secs(5))
        .await
        .expect("tab B never received tab A's update over the broadcast");
    let doc_b = NotedDoc::new();
    doc_b
        .apply_update(&relayed)
        .expect("relayed bytes were not a valid CRDT update");
    assert_eq!(
        doc_b.text_for_test(),
        "alpha",
        "tab B's document should hold tab A's edit"
    );

    // Tab A must NOT be echoed its own update back.
    assert!(
        next_update(&mut a, Duration::from_millis(750))
            .await
            .is_none(),
        "the originating session was echoed its own update"
    );

    // Tab B now types on top of A's edit, exactly as a real second tab would.
    let update_b = doc_b.append_paragraph_for_test("beta");
    send_update(&mut b, update_b).await;

    // Closing both sockets flushes the debounced projection.
    a.close(None).await.ok();
    b.close(None).await.ok();

    let texts = await_block_texts(&pool, page_id, &["alpha", "beta"]).await;
    assert_eq!(
        texts,
        vec!["alpha", "beta"],
        "blocks must hold BOTH sessions' content; one session clobbered the other"
    );

    // The last session out must drop the hub, or the registry leaks one entry
    // per page ever opened.
    for _ in 0..100 {
        if state.hub_count().await == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        state.hub_count().await,
        0,
        "the hub must be removed once the last session disconnects"
    );
}

/// `blocks` is a DERIVED projection of `doc_updates`, which is the source of
/// truth. That invariant was stated but never exercised, and projection
/// failures are logged and ignored — so a page could sit stale forever with
/// nothing able to repair it, while M1b indexes off the table.
///
/// This is the first real proof the invariant holds: drive a genuine edit
/// through the WebSocket, corrupt the projection behind the server's back with
/// raw SQL, and show the endpoint rebuilds it from the log alone.
#[tokio::test]
async fn reproject_repairs_a_corrupted_projection() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let (pool, page_id) = setup().await;
    let addr = serve(noted_server::app(AppState::new_for_test(pool.clone()))).await;

    // Drive a real update through the real sync path so `blocks` is populated
    // the way production populates it.
    let mut ws = connect(addr, page_id).await;
    let doc = NotedDoc::new();
    send_update(&mut ws, doc.append_paragraph_for_test("the original text")).await;
    send_update(&mut ws, doc.append_node_for_test("heading", "a heading")).await;
    ws.close(None).await.ok();

    let healthy = await_block_texts(&pool, page_id, &["the original text", "a heading"]).await;
    assert_eq!(
        healthy,
        vec!["the original text", "a heading"],
        "precondition: the session must project the edit before we corrupt it"
    );
    let expected = noted_db::blocks::for_page(&pool, page_id).await.unwrap();

    // Corrupt the projection behind the server's back: drop one row and
    // garble the other. The log is untouched, so a correct rebuild must
    // restore both.
    sqlx::query("DELETE FROM blocks WHERE page_id = $1 AND block_index = 1")
        .bind(page_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE blocks SET text = 'CORRUPTED', content_hash = 'deadbeef' WHERE page_id = $1",
    )
    .bind(page_id)
    .execute(&pool)
    .await
    .unwrap();

    let corrupted = noted_db::blocks::for_page(&pool, page_id).await.unwrap();
    assert_ne!(
        corrupted, expected,
        "precondition: blocks must be corrupt now"
    );

    // Repair.
    let res = noted_server::app(AppState::new_for_test(pool.clone()))
        .oneshot(
            Request::builder().header("cookie", common::cookie_header())
                .method("POST")
                .uri(format!("/api/pages/{page_id}/reproject"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["blocks"], 2);

    // The projection must now match what the CRDT log actually says.
    let repaired = noted_db::blocks::for_page(&pool, page_id).await.unwrap();
    assert_eq!(
        repaired, expected,
        "reproject must rebuild blocks to match the CRDT state"
    );

    let from_log = NotedDoc::from_updates(&docs::load(&pool, page_id).await.unwrap())
        .unwrap()
        .project();
    assert_eq!(
        repaired, from_log,
        "blocks must be exactly the projection of the doc_updates log"
    );
}

#[tokio::test]
async fn reproject_unknown_page_returns_404() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let (pool, _page_id) = setup().await;
    let res = noted_server::app(AppState::new_for_test(pool))
        .oneshot(
            Request::builder().header("cookie", common::cookie_header())
                .method("POST")
                .uri(format!("/api/pages/{}/reproject", uuid::Uuid::new_v4()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

/// A failed log write must leave the SHARED document untouched.
///
/// The session closes the socket when `docs::append` fails, on the premise that
/// the client still holds the edit and will re-send it on reconnect. That
/// premise only survives if the server's document does NOT hold the update:
/// the shared doc outlives the session, so an update applied but never logged
/// would sit in the doc's state vector forever, the reconnect handshake would
/// conclude the server already had the edit, and it would be lost from the log
/// for good — with `blocks` projected from state the log cannot reproduce.
///
/// Appending is deliberately ordered BEFORE applying to prevent that. This test
/// fails if the order is ever swapped back.
#[tokio::test]
async fn a_failed_append_does_not_mutate_the_shared_doc() {
    let (pool, page_id) = setup().await;
    let state = AppState::new_for_test(pool.clone());
    let addr = serve(noted_server::app(state.clone())).await;

    // A second session keeps the hub (and its doc) alive after the writer's
    // session dies, which is exactly the case where divergence would persist.
    let mut keepalive = connect(addr, page_id).await;
    let mut writer = connect(addr, page_id).await;

    // Force appends to fail: doc_updates and doc_seq both FK to pages, so
    // removing the page makes the insert violate the constraint.
    sqlx::query("DELETE FROM pages WHERE id = $1")
        .bind(page_id)
        .execute(&pool)
        .await
        .unwrap();

    let doc = NotedDoc::new();
    send_update(
        &mut writer,
        doc.append_paragraph_for_test("must not survive"),
    )
    .await;

    // The server closing the socket is our signal that the append failed and
    // the session bailed — no sleeping on a guessed duration.
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(frame)) = writer.next().await {
            if matches!(frame, WsMessage::Close(_)) {
                return true;
            }
        }
        true // stream ended = socket closed
    })
    .await;
    assert!(
        closed.is_ok(),
        "the session must close its socket when the append fails"
    );

    // Nothing reached the log...
    assert!(
        docs::load(&pool, page_id).await.unwrap().is_empty(),
        "precondition: the append must actually have failed"
    );

    // ...so the shared doc must not hold the edit either.
    let hubs = state.hubs.lock().await;
    let hub = hubs
        .get(&page_id)
        .expect("the keepalive session should still hold the hub");
    let text = hub.doc.lock().await.text_for_test();
    assert_eq!(
        text, "",
        "the shared doc absorbed an update that was never logged; \
         the client's reconnect handshake will never re-send it"
    );

    drop(hubs);
    keepalive.close(None).await.ok();
}

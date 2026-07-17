use futures_util::SinkExt;
use noted_crdt::NotedDoc;
use noted_db::docs;
use noted_server::AppState;
use noted_server::routes::sync::{SyncMsg, encode_msg, parse_msg};

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

/// Sets up a workspace + page directly via sqlx, mirroring the `setup()`
/// helper in `crates/noted-db/tests/docs.rs`.
async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
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
    let app = noted_server::app(AppState { pool: pool.clone() });
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let url = format!("ws://{addr}/sync/{page_id}");
    let (mut ws_stream, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("failed to connect to sync websocket");

    // Real CRDT update produced by the same NotedDoc machinery the server
    // uses, so this is a realistic y-protocols Update payload, not a stub.
    let doc = NotedDoc::new();
    let update = doc.append_paragraph_for_test("hello over the wire");

    let frame = encode_msg(&SyncMsg::Update(update));
    ws_stream
        .send(tokio_tungstenite::tungstenite::Message::Binary(
            frame.into(),
        ))
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

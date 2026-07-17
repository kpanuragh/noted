use noted_crdt::NotedDoc;
use noted_server::routes::sync::{encode_msg, parse_msg, SyncMsg};

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
    assert!(parse_msg(&[0, 99, 1]).is_none(), "unknown subtype must not panic");
    assert!(parse_msg(&[7, 0, 1]).is_none(), "non-sync message must not panic");
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

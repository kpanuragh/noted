use noted_crdt::NotedDoc;

/// Applying a stream of updates must produce the same state as applying the
/// single compacted update that represents them. This is the property the
/// update-log compaction in Task 7 depends on.
#[test]
fn compaction_preserves_state() {
    let a = NotedDoc::new();
    let mut updates = Vec::new();
    for word in ["alpha", "beta", "gamma"] {
        updates.push(a.append_paragraph_for_test(word));
    }

    let replayed = NotedDoc::from_updates(&updates).unwrap();
    let compacted_bytes = replayed.encode_full();
    let compacted = NotedDoc::from_updates(&[compacted_bytes]).unwrap();

    assert_eq!(replayed.text_for_test(), compacted.text_for_test());
    assert_eq!(replayed.text_for_test(), "alpha\nbeta\ngamma");
}

#[test]
fn diff_against_empty_state_vector_returns_everything() {
    let doc = NotedDoc::new();
    doc.append_paragraph_for_test("hello");

    let empty = NotedDoc::new();
    let diff = doc.diff(&empty.state_vector()).unwrap();

    let rebuilt = NotedDoc::new();
    rebuilt.apply_update(&diff).unwrap();
    assert_eq!(rebuilt.text_for_test(), "hello");
}

#[test]
fn concurrent_edits_converge() {
    let a = NotedDoc::new();
    let b = NotedDoc::new();

    let ua = a.append_paragraph_for_test("from-a");
    let ub = b.append_paragraph_for_test("from-b");

    a.apply_update(&ub).unwrap();
    b.apply_update(&ua).unwrap();

    assert_eq!(a.text_for_test(), b.text_for_test(), "CRDT replicas must converge");
}

#[test]
fn garbage_update_is_an_error_not_a_panic() {
    let doc = NotedDoc::new();
    let err = doc.apply_update(&[0xff, 0xff, 0xff, 0xff]);
    assert!(err.is_err(), "malformed updates from the network must not panic the server");
}

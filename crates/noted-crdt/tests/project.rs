use noted_crdt::NotedDoc;

#[test]
fn projects_each_top_level_node_to_a_block() {
    let doc = NotedDoc::new();
    doc.append_paragraph_for_test("first");
    doc.append_paragraph_for_test("second");

    let blocks = doc.project();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].index, 0);
    assert_eq!(blocks[0].text, "first");
    assert_eq!(blocks[0].node_type, "paragraph");
    assert_eq!(blocks[1].index, 1);
    assert_eq!(blocks[1].text, "second");
}

#[test]
fn content_hash_is_stable_for_identical_text() {
    let a = NotedDoc::new();
    a.append_paragraph_for_test("same");
    let b = NotedDoc::new();
    b.append_paragraph_for_test("same");

    assert_eq!(a.project()[0].content_hash, b.project()[0].content_hash);
}

#[test]
fn content_hash_changes_when_text_changes() {
    let a = NotedDoc::new();
    a.append_paragraph_for_test("before");
    let b = NotedDoc::new();
    b.append_paragraph_for_test("after");

    assert_ne!(a.project()[0].content_hash, b.project()[0].content_hash);
}

#[test]
fn content_hash_differs_for_same_text_different_node_type() {
    let a = NotedDoc::new();
    a.append_node_for_test("paragraph", "same words");
    let b = NotedDoc::new();
    b.append_node_for_test("heading", "same words");

    let a_block = &a.project()[0];
    let b_block = &b.project()[0];
    assert_eq!(a_block.text, b_block.text);
    assert_ne!(
        a_block.node_type, b_block.node_type,
        "test setup sanity check: node types must differ"
    );
    assert_ne!(
        a_block.content_hash, b_block.content_hash,
        "changing node_type (e.g. paragraph -> heading) with identical text must change the content hash, \
         otherwise M1b's skip-unchanged-blocks optimisation would miss real structural changes"
    );
}

#[test]
fn empty_document_projects_to_no_blocks() {
    assert!(NotedDoc::new().project().is_empty());
}

/// The brief's reference implementation used `.get_string()` on the XML node
/// directly, which returns tag-wrapped markup (e.g. `"<paragraph>alpha</paragraph>"`)
/// rather than plain text — the same bug fixed in Task 6's `text_for_test`.
/// If `blocks.text` ever regresses to holding markup, M1b would chunk/embed
/// XML tags and M1c's FTS index would index `<paragraph>` instead of words,
/// with every other test in this file still passing. This test exists solely
/// to make that regression visible.
#[test]
fn projected_text_is_plain_not_xml_markup() {
    let doc = NotedDoc::new();
    doc.append_paragraph_for_test("alpha");

    let blocks = doc.project();
    assert_eq!(blocks[0].text, "alpha", "projected text must be exactly the plain text");
    assert!(
        !blocks[0].text.contains('<') && !blocks[0].text.contains('>'),
        "projected text must not contain XML tag markup, got: {:?}",
        blocks[0].text
    );
}

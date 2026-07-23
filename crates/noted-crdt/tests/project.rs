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
    assert_eq!(
        blocks[0].text, "alpha",
        "projected text must be exactly the plain text"
    );
    assert!(
        !blocks[0].text.contains('<') && !blocks[0].text.contains('>'),
        "projected text must not contain XML tag markup, got: {:?}",
        blocks[0].text
    );
}

/// Inline formatting must NOT reach the projected text.
///
/// `XmlText::get_string` serializes formatting marks as XML tags, so a bold run
/// projected as `<bold>Dinosaurs</bold>` and a pasted link as
/// `<link href="..." class="null" title="...">reptiles</link>`. That markup was
/// written to `blocks.text`, which is the input to the full-text index, to the
/// chunks that get embedded, and to the text handed to the extraction model.
/// The visible symptom was raw tags in search snippets; the invisible ones were
/// FTS matching on `href`, embeddings encoding Wikipedia URLs, and the graph
/// extracting entities out of markup.
///
/// Every fixture in this file used unformatted text, so nothing caught it.
///
/// MECHANISM PROTECTED: the `diff`-based `XmlOut::Text` arm of `plain_text`.
/// Restore `get_string` there and this test fails.
#[test]
fn inline_formatting_marks_never_reach_the_projected_text() {
    let doc = NotedDoc::new();
    doc.append_formatted_paragraph_for_test(&[
        ("Dinosaurs", &[("bold", "true")]),
        (" are a diverse group of ", &[]),
        (
            "reptiles",
            &[
                ("link", "true"),
                ("href", "https://en.wikipedia.org/wiki/Reptile"),
                ("class", "null"),
                ("title", "Reptile"),
            ],
        ),
        (".", &[]),
    ]);

    let text = &doc.project()[0].text;

    assert_eq!(
        text, "Dinosaurs are a diverse group of reptiles.",
        "the projection must be the words, not the markup"
    );
    // Stated separately so a failure says WHICH leak happened.
    assert!(!text.contains('<'), "a tag leaked into the text: {text}");
    assert!(
        !text.contains("href") && !text.contains("wikipedia"),
        "link attributes leaked into the searchable text: {text}"
    );
    assert!(
        !text.contains("bold") && !text.contains("class"),
        "mark names leaked into the searchable text: {text}"
    );
}

/// The text still has to survive intact — a fix that stripped the formatting by
/// dropping the formatted RUNS would pass every assertion above.
#[test]
fn formatted_runs_keep_their_words() {
    let doc = NotedDoc::new();
    doc.append_formatted_paragraph_for_test(&[
        ("plain ", &[]),
        ("emphasised", &[("italic", "true")]),
        (" tail", &[]),
    ]);
    assert_eq!(doc.project()[0].text, "plain emphasised tail");
}

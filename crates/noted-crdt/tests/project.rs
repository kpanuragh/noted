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

/// Structured blocks must not run their parts together.
///
/// `plain_text` concatenates child text with NO separator, which is right
/// inside a paragraph — "Hello " + "world" is one sentence — and wrong for a
/// list or a table, where each item is its own phrase. Joined blind, a
/// two-item list projects as "first itemsecond item" and a table row as
/// "NameAge": tokens that appear in no dictionary, match no query, and embed
/// as noise.
///
/// This matters before offering those blocks in the editor at all. A block the
/// writer can insert but the index cannot read is worse than one that does not
/// exist.
#[test]
fn a_list_projects_with_its_items_separated() {
    let doc = NotedDoc::new();
    doc.append_list_for_test("bulletList", &["first item", "second item"]);

    let text = &doc.project()[0].text;
    assert!(
        !text.contains("itemsecond"),
        "list items ran together: {text:?}"
    );
    assert!(text.contains("first item"), "{text:?}");
    assert!(text.contains("second item"), "{text:?}");
}

#[test]
fn a_table_projects_with_its_cells_separated() {
    let doc = NotedDoc::new();
    doc.append_table_for_test(&[["Name", "Role"], ["Priya", "pricing"]]);

    let text = &doc.project()[0].text;
    assert!(!text.contains("NameRole"), "cells ran together: {text:?}");
    assert!(text.contains("Name"), "{text:?}");
    assert!(text.contains("pricing"), "{text:?}");
}

/// Blocks this project defines itself must project as readably as built-in
/// ones — a callout's paragraphs separated, a toggle's title kept with, but
/// not welded to, its body.
///
/// Custom node types are exactly where a projection quietly stops working:
/// they are unknown to the walker, so whatever it does by DEFAULT is what they
/// get. The default here is "treat an unknown tag as a block and separate it",
/// which is why these pass without naming callout or toggle anywhere in the
/// projection code.
#[test]
fn custom_blocks_project_with_their_parts_separated() {
    let doc = NotedDoc::new();
    doc.append_nested_for_test(
        "callout",
        &[("paragraph", "first line"), ("paragraph", "second line")],
    );
    let callout = &doc.project()[0].text;
    assert!(!callout.contains("linesecond"), "callout ran together: {callout:?}");

    let doc2 = NotedDoc::new();
    doc2.append_nested_for_test(
        "details",
        &[("summary", "Deploy steps"), ("div", "run the migration")],
    );
    let toggle = &doc2.project()[0].text;
    assert!(
        !toggle.contains("stepsrun"),
        "toggle summary welded to its body: {toggle:?}"
    );
    assert!(toggle.contains("Deploy steps") && toggle.contains("run the migration"));
}

/// Columns must not weld their halves together.
///
/// Two columns are two separate trains of thought placed side by side, not one
/// sentence — "Left sideRight side" is exactly the token the block separator
/// exists to prevent, and columns are the block most likely to produce it since
/// their children sit adjacent by definition.
#[test]
fn columns_project_with_their_sides_separated() {
    let doc = NotedDoc::new();
    doc.append_nested_for_test("div", &[("div", "Left side"), ("div", "Right side")]);
    let text = &doc.project()[0].text;
    assert!(!text.contains("sideRight"), "columns ran together: {text:?}");
    assert!(text.contains("Left side") && text.contains("Right side"));
}

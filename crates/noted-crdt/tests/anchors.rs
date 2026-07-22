//! M5-1 — comment anchors that survive concurrent edits.
use noted_crdt::{NotedDoc, Resolved};

/// A doc with one paragraph, and an anchor into the middle of it.
fn doc_with_anchor(text: &str, offset: u32) -> (NotedDoc, noted_crdt::Anchor) {
    let doc = NotedDoc::new();
    doc.append_paragraph_for_test(text);
    let anchor = doc.anchor_at(0, offset).expect("anchor must be creatable");
    (doc, anchor)
}

#[test]
fn an_anchor_resolves_where_it_was_made() {
    let (doc, anchor) = doc_with_anchor("the quick brown fox", 4);
    assert_eq!(doc.resolve(&anchor), Resolved::At { block: 0, offset: 4 });
}

/// **The headline property: an anchor FOLLOWS its text when someone edits
/// above it.**
///
/// A character offset would now point at the wrong words — silently. This is
/// the failure the issue names as unacceptable.
///
/// MECHANISM PROTECTED: using `StickyIndex` at all. Store a bare offset instead
/// and this fails, because 4 no longer names the same place.
#[test]
fn an_anchor_follows_its_text_when_earlier_text_is_inserted() {
    let (doc, anchor) = doc_with_anchor("the quick brown fox", 4);
    assert_eq!(doc.resolve(&anchor), Resolved::At { block: 0, offset: 4 });

    // Another session inserts at the very start of the same paragraph.
    doc.insert_text_for_test(0, 0, "PREFIX ");

    match doc.resolve(&anchor) {
        Resolved::At { offset, .. } => assert_eq!(
            offset, 11,
            "the anchor must move with its text: 4 + len(\"PREFIX \")"
        ),
        Resolved::Orphaned => panic!("an insert above must not orphan an anchor"),
    }
}

/// An edit AFTER the anchor leaves it alone.
#[test]
fn an_anchor_is_unmoved_by_later_text() {
    let (doc, anchor) = doc_with_anchor("the quick brown fox", 4);
    doc.insert_text_for_test(0, 19, " jumps");
    assert_eq!(doc.resolve(&anchor), Resolved::At { block: 0, offset: 4 });
}

/// **Deleting the anchored text ORPHANS the comment rather than moving it
/// somewhere plausible.**
///
/// Re-pointing a review note at whatever survived nearby is worse than losing
/// it, because the reader has no way to know it moved.
#[test]
fn deleting_the_anchored_text_orphans_the_comment() {
    let doc = NotedDoc::new();
    doc.append_paragraph_for_test("the quick brown fox");
    let anchor = doc.anchor_at(0, 10).unwrap();
    assert!(matches!(doc.resolve(&anchor), Resolved::At { .. }), "premise");

    // Remove the whole paragraph's text.
    doc.delete_text_for_test(0, 0, 19);

    assert_eq!(
        doc.resolve(&anchor),
        Resolved::Orphaned,
        "the anchor must report orphaned, never a nearby offset"
    );
}

/// Two concurrent sessions: one comments, the other edits, and the comment
/// still names the right words after they merge.
///
/// This is the real scenario — the anchor is created on one replica and
/// resolved on another that has seen a different edit history.
#[test]
fn an_anchor_survives_a_merge_from_a_concurrent_replica() {
    let a = NotedDoc::new();
    let update = a.append_paragraph_for_test("alpha beta gamma");

    // B starts from A's state, then edits independently.
    let b = NotedDoc::new();
    b.apply_update(&update).unwrap();
    let b_edit = b.insert_text_for_test(0, 0, "ZZ ");

    // A anchors at "beta" (offset 6) without having seen B's edit.
    let anchor = a.anchor_at(0, 6).unwrap();
    assert_eq!(a.resolve(&anchor), Resolved::At { block: 0, offset: 6 });

    // Now A receives B's edit.
    a.apply_update(&b_edit).unwrap();

    match a.resolve(&anchor) {
        Resolved::At { offset, .. } => assert_eq!(
            offset, 9,
            "after merging a 3-char insert at the start, the anchor must be at 6+3"
        ),
        Resolved::Orphaned => panic!("a concurrent edit elsewhere must not orphan it"),
    }
}

/// Anchoring to a block that does not exist fails rather than inventing a
/// position.
#[test]
fn anchoring_to_a_missing_block_fails_rather_than_guessing() {
    let doc = NotedDoc::new();
    doc.append_paragraph_for_test("only block");
    assert!(doc.anchor_at(7, 0).is_none());
}

/// A corrupt anchor resolves to orphaned rather than panicking — a comment row
/// whose bytes were truncated must not take down the page.
#[test]
fn a_corrupt_anchor_is_orphaned_not_a_panic() {
    let (doc, _) = doc_with_anchor("text", 1);
    let bad = noted_crdt::Anchor {
        block: 0,
        encoded: vec![0xff, 0x00, 0x13],
        quote: String::new(),
    };
    assert_eq!(doc.resolve(&bad), Resolved::Orphaned);
}

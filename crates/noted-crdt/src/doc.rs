use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Assoc, IndexedSequence, StickyIndex, Text as YText, XmlOut,
    
    Doc, GetString, ReadTxn, StateVector, Transact, Update, XmlElementPrelim, XmlFragment,
    XmlFragmentRef, XmlTextPrelim,
};

/// The root name of the ProseMirror fragment. Must match the client's
/// `Collaboration.configure({ field: "prosemirror" })` in Task 10.
pub const ROOT: &str = "prosemirror";

#[derive(Debug, thiserror::Error)]
pub enum CrdtError {
    #[error("failed to decode update: {0}")]
    Decode(String),
}

pub struct NotedDoc {
    doc: Doc,
}

impl Default for NotedDoc {
    fn default() -> Self {
        Self::new()
    }
}

impl NotedDoc {
    pub fn new() -> Self {
        Self { doc: Doc::new() }
    }

    fn fragment(&self) -> XmlFragmentRef {
        self.doc.get_or_insert_xml_fragment(ROOT)
    }

    /// Exposes the underlying `Doc` to sibling modules within this crate
    /// (namely `project`), so they can open their own transactions rather
    /// than duplicating the fragment-walking logic here.
    pub(crate) fn doc_pub(&self) -> &Doc {
        &self.doc
    }

    /// Exposes the root ProseMirror fragment to sibling modules. See
    /// `doc_pub`.
    pub(crate) fn fragment_pub(&self) -> XmlFragmentRef {
        self.fragment()
    }

    pub fn from_updates(updates: &[Vec<u8>]) -> Result<Self, CrdtError> {
        let d = Self::new();
        for u in updates {
            d.apply_update(u)?;
        }
        Ok(d)
    }

    /// Check that `update` decodes, WITHOUT applying it to any document.
    ///
    /// Lets a server reject malformed input before writing it to the log, and
    /// then apply it only once the log write has succeeded. That ordering
    /// matters for a document shared between sessions: such a document outlives
    /// any one session, so an update applied to it but missing from the log
    /// would be advertised in its state vector forever — the client's reconnect
    /// handshake would conclude the server already had the edit and never
    /// re-send it, losing it silently.
    ///
    /// The update is decoded again by the subsequent `apply_update`. The
    /// decoded value is deliberately not returned: `yrs::Update` is not `Send`,
    /// so it could not be held across the caller's `.await` points anyway, and
    /// decoding a keystroke-sized update twice is far cheaper than the database
    /// round trip it guards.
    pub fn validate_update(update: &[u8]) -> Result<(), CrdtError> {
        Update::decode_v1(update)
            .map(|_| ())
            .map_err(|e| CrdtError::Decode(e.to_string()))
    }

    pub fn apply_update(&self, update: &[u8]) -> Result<(), CrdtError> {
        let update = Update::decode_v1(update).map_err(|e| CrdtError::Decode(e.to_string()))?;
        let mut txn = self.doc.transact_mut();
        txn.apply_update(update)
            .map_err(|e| CrdtError::Decode(e.to_string()))?;
        Ok(())
    }

    pub fn state_vector(&self) -> Vec<u8> {
        self.doc.transact().state_vector().encode_v1()
    }

    pub fn diff(&self, state_vector: &[u8]) -> Result<Vec<u8>, CrdtError> {
        let sv =
            StateVector::decode_v1(state_vector).map_err(|e| CrdtError::Decode(e.to_string()))?;
        Ok(self.doc.transact().encode_diff_v1(&sv))
    }

    pub fn encode_full(&self) -> Vec<u8> {
        self.doc.transact().encode_diff_v1(&StateVector::default())
    }

    /// Append a `<paragraph>` containing `text`, returning the update bytes it
    /// produced. Test-only: the real client drives edits through y-prosemirror.
    pub fn append_paragraph_for_test(&self, text: &str) -> Vec<u8> {
        self.append_node_for_test("paragraph", text)
    }

    /// Append an XML element with the given tag name containing `text`,
    /// returning the update bytes it produced. Test-only: lets tests exercise
    /// node types other than `paragraph` (e.g. `heading`) without pulling in
    /// y-prosemirror.
    pub fn append_node_for_test(&self, tag: &str, text: &str) -> Vec<u8> {
        let frag = self.fragment();
        let before = self.doc.transact().state_vector();
        {
            let mut txn = self.doc.transact_mut();
            let len = frag.len(&txn);
            let node = frag.insert(&mut txn, len, XmlElementPrelim::empty(tag));
            node.insert(&mut txn, 0, XmlTextPrelim::new(text));
        }
        self.doc.transact().encode_diff_v1(&before)
    }

    /// Newline-joined text of each top-level node. Test-only mirror of the
    /// projection logic in Task 11.
    pub fn text_for_test(&self) -> String {
        let frag = self.fragment();
        let txn = self.doc.transact();
        (0..frag.len(&txn))
            .filter_map(|i| frag.get(&txn, i))
            .map(|node| plain_text(&node, &txn, 0))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// XML nesting depth beyond which `plain_text` stops descending. This walk is
/// on the production projection path (`project.rs`) fed by arbitrarily nested
/// XML from untrusted WebSocket clients, so an unbounded recursive descent
/// could be used to overflow the stack. 64 levels is far deeper than any
/// legitimate ProseMirror document produces.
const MAX_DEPTH: usize = 64;

/// Recursively extracts the plain (non-XML-tagged) text content of an XML
/// node, concatenating the text of all descendant text nodes in document
/// order. `get_string` on `XmlElementRef`/`XmlFragmentRef` returns the XML
/// serialization (with tags), which is not what callers want.
///
/// `pub(crate)` so `project.rs` reuses this rather than duplicating the walk.
pub(crate) fn plain_text<T: ReadTxn>(
    node: &yrs::types::xml::XmlOut,
    txn: &T,
    depth: usize,
) -> String {
    use yrs::types::xml::XmlOut;
    if depth >= MAX_DEPTH {
        tracing::debug!(
            depth,
            MAX_DEPTH,
            "plain_text: max XML nesting depth reached; dropping remaining descendants"
        );
        return String::new();
    }
    match node {
        XmlOut::Text(t) => t.get_string(txn),
        XmlOut::Element(e) => e
            .children(txn)
            .map(|child| plain_text(&child, txn, depth + 1))
            .collect::<Vec<_>>()
            .join(""),
        XmlOut::Fragment(f) => f
            .children(txn)
            .map(|child| plain_text(&child, txn, depth + 1))
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// How many characters an anchor quotes by default.
///
/// Long enough to be distinctive within a paragraph, short enough that ordinary
/// editing NEAR a comment does not orphan it. A comment survives edits after
/// its quote and before its position; it orphans when the quoted text itself
/// changes, which is the honest reading of "the thing I commented on is gone".
pub const QUOTE_LEN: usize = 24;

/// A comment anchor: a position in the document that survives concurrent edits.
///
/// # Why not a character offset
///
/// A comment stored as "characters 40..55 of block 2" is wrong the moment
/// anyone inserts text above it — and wrong SILENTLY, pointing at whatever
/// words happen to occupy those offsets afterwards. That is worse than losing
/// the comment: a review note attached to the wrong sentence actively misleads
/// the person reading it.
///
/// `yrs::StickyIndex` is the CRDT's own answer. It names a position relative to
/// the ITEMS around it rather than to a count, so concurrent inserts and
/// deletes carry it along with the text. When the text it named is deleted
/// outright, resolving fails — reported as ORPHANED rather than silently
/// clamped to whatever is nearby.
///
/// # Known limit, stated rather than hidden
///
/// The anchor tracks the OFFSET WITHIN a block, not which block. `block` is
/// stored alongside and is a plain index, so reordering top-level blocks can
/// leave an anchor naming the wrong one. Text editing — the overwhelmingly
/// common case, and the one the issue's acceptance names — is fully tracked.
/// Fixing block identity needs a stable id per block, which the ProseMirror
/// schema does not currently carry; recorded here rather than papered over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// Which top-level block. See the limit above.
    pub block: usize,
    /// The encoded `StickyIndex`, opaque bytes as far as the database is
    /// concerned.
    pub encoded: Vec<u8>,
    /// The text this comment was attached to.
    ///
    /// # Why a quote is needed as well as a sticky index
    ///
    /// `StickyIndex` alone is NOT enough, and the test caught it: when the item
    /// it names is deleted, yrs does not fail — it CLAMPS to the nearest
    /// surviving position, which for a emptied paragraph is offset 0. A comment
    /// on a deleted sentence would silently reappear attached to the start of
    /// whatever text remains, which is exactly the failure this design exists
    /// to prevent.
    ///
    /// So the anchor also remembers what it was pointing AT, and resolution
    /// checks the text is still there. This is how Google Docs and GitHub
    /// detect orphaned comments too — the position tells you where, and the
    /// quote tells you whether "where" still means anything.
    pub quote: String,
}

/// Where an anchor resolves to now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// The character offset the anchor currently names.
    At { block: usize, offset: u32 },
    /// The text this anchor named is gone. The comment still exists and must be
    /// SHOWN as orphaned — never re-pointed at whatever happens to be nearby.
    Orphaned,
}

impl NotedDoc {
    /// Create an anchor at `offset` within the text of block `block`.
    ///
    /// `None` when the block does not exist or holds no text: a caller cannot
    /// anchor to something that is not there, and inventing a position would
    /// produce exactly the silent-wrong-place failure this type prevents.
    pub fn anchor_at(&self, block: usize, offset: u32) -> Option<Anchor> {
        self.anchor_to(block, offset, QUOTE_LEN)
    }

    /// As `anchor_at`, quoting `quote_len` characters from the position.
    pub fn anchor_to(&self, block: usize, offset: u32, quote_len: usize) -> Option<Anchor> {
        let text = self.block_text(block)?;
        let quote: String = {
            let txn = self.doc.transact();
            let full = text.get_string(&txn);
            full.chars()
                .skip(offset as usize)
                .take(quote_len)
                .collect()
        };
        let mut txn = self.doc.transact_mut();
        let sticky = text.sticky_index(&mut txn, offset, Assoc::After)?;
        Some(Anchor {
            block,
            encoded: sticky.encode_v1(),
            quote,
        })
    }

    /// Where does this anchor point now?
    pub fn resolve(&self, anchor: &Anchor) -> Resolved {
        let Ok(sticky) = StickyIndex::decode_v1(&anchor.encoded) else {
            return Resolved::Orphaned;
        };
        let offset = {
            let txn = self.doc.transact();
            match sticky.get_offset(&txn) {
                Some(abs) => abs.index,
                None => return Resolved::Orphaned,
            }
        };

        // The position resolved — but to WHAT? `StickyIndex` clamps rather than
        // failing when its item is deleted, so a position alone can point at
        // text the comment was never about. Verify the quote is still there.
        if !anchor.quote.is_empty() {
            let Some(text) = self.block_text(anchor.block) else {
                return Resolved::Orphaned;
            };
            let txn = self.doc.transact();
            let current: String = text
                .get_string(&txn)
                .chars()
                .skip(offset as usize)
                .take(anchor.quote.chars().count())
                .collect();
            if current != anchor.quote {
                return Resolved::Orphaned;
            }
        }

        Resolved::At {
            block: anchor.block,
            offset,
        }
    }
}

impl NotedDoc {
    /// Insert text into a block. Test-only: production text edits arrive as
    /// CRDT updates from the editor.
    pub fn insert_text_for_test(&self, block: usize, offset: u32, text: &str) -> Vec<u8> {
        let frag = self.fragment();
        let before = self.doc.transact().state_vector();
        {
            if let Some(t) = self.block_text(block) {
                let mut txn = self.doc.transact_mut();
                t.insert(&mut txn, offset, text);
            }
        }
        self.doc.transact().encode_diff_v1(&before)
    }

    /// Delete a range of text from a block. Test-only.
    pub fn delete_text_for_test(&self, block: usize, offset: u32, len: u32) -> Vec<u8> {
        let frag = self.fragment();
        let before = self.doc.transact().state_vector();
        {
            if let Some(t) = self.block_text(block) {
                let mut txn = self.doc.transact_mut();
                t.remove_range(&mut txn, offset, len);
            }
        }
        self.doc.transact().encode_diff_v1(&before)
    }
}

impl NotedDoc {
    /// The `XmlText` inside top-level block `block`.
    ///
    /// A block is an ELEMENT (`<paragraph>`), and its text is a child — so
    /// `fragment.get(i)` yields the element, not the text. The first version of
    /// the anchor code matched `XmlOut::Text` on the block itself and therefore
    /// never found anything, which is why every anchor test failed at once.
    fn block_text(&self, block: usize) -> Option<yrs::XmlTextRef> {
        let frag = self.fragment();
        let txn = self.doc.transact();
        match frag.get(&txn, block as u32)? {
            // Already text (a bare text node at top level).
            XmlOut::Text(t) => Some(t),
            // The normal case: descend into the element for its first text
            // child.
            XmlOut::Element(el) => (0..el.len(&txn)).find_map(|i| match el.get(&txn, i) {
                Some(XmlOut::Text(t)) => Some(t),
                _ => None,
            }),
            _ => None,
        }
    }
}

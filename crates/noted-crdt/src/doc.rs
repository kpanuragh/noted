use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
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

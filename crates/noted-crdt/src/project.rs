use sha2::{Digest, Sha256};
use yrs::{Transact, XmlFragment};

use crate::doc::{plain_text, NotedDoc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedBlock {
    pub index: i32,
    pub node_type: String,
    pub text: String,
    /// SHA-256 of the text. Content addressing so M1b can skip unchanged
    /// blocks — see spec §6.2.
    pub content_hash: String,
}

pub fn content_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

impl NotedDoc {
    /// Flatten the CRDT document into one block per top-level node.
    ///
    /// Text is extracted with `plain_text`, NOT `.get_string()` — the latter
    /// returns XML-tag-wrapped markup (e.g. `"<paragraph>alpha</paragraph>"`)
    /// for element/fragment nodes, which would poison the `blocks` table that
    /// M1b (chunking/embeddings) and M1c (full-text/vector search) consume.
    pub fn project(&self) -> Vec<ProjectedBlock> {
        let frag = self.fragment_pub();
        let txn = self.doc_pub().transact();
        (0..frag.len(&txn))
            .filter_map(|i| frag.get(&txn, i).map(|node| (i, node)))
            .map(|(i, node)| {
                let node_type = match &node {
                    yrs::types::xml::XmlOut::Element(e) => e.tag().to_string(),
                    yrs::types::xml::XmlOut::Text(_) => "text".to_string(),
                    yrs::types::xml::XmlOut::Fragment(_) => "fragment".to_string(),
                };
                let text = plain_text(&node, &txn, 0);
                ProjectedBlock {
                    index: i as i32,
                    content_hash: content_hash(&text),
                    node_type,
                    text,
                }
            })
            .collect()
    }
}

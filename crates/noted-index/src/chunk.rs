use sha2::{Digest, Sha256};

/// Below this, a block is merged forward — a bare heading embeds poorly.
pub const MIN_TOKENS: i32 = 64;
/// Above this, a block is split. Roughly the practical window of the model.
pub const MAX_TOKENS: i32 = 512;

#[derive(Debug, Clone)]
pub struct SourceBlock {
    pub node_type: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    pub content_hash: String,
    pub token_estimate: i32,
}

/// Whitespace-delimited word count scaled by a fudge factor. Deliberately not a
/// real tokenizer: this only picks split points, and being wrong by 20% costs
/// nothing. A real tokenizer would add a dependency and a model coupling for no
/// benefit.
pub fn estimate_tokens(text: &str) -> i32 {
    let words = text.split_whitespace().count() as f32;
    (words * 1.3).ceil() as i32
}

fn hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

fn make(text: String) -> Chunk {
    let token_estimate = estimate_tokens(&text);
    Chunk { content_hash: hash(&text), text, token_estimate }
}

/// Split a too-long text at sentence boundaries, keeping every piece under
/// MAX_TOKENS. Falls back to a hard word-split for text with no sentence
/// terminators (e.g. a wall of prose with no full stops).
fn split_long(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();

    for sentence in text.split_inclusive(['.', '!', '?']) {
        if estimate_tokens(sentence) > MAX_TOKENS {
            if !current.trim().is_empty() {
                out.push(std::mem::take(&mut current));
            }
            // No sentence boundary helps here — split on words.
            let words: Vec<&str> = sentence.split_whitespace().collect();
            let per = (MAX_TOKENS as f32 / 1.3).floor() as usize;
            for w in words.chunks(per.max(1)) {
                out.push(w.join(" "));
            }
            continue;
        }
        if estimate_tokens(&(current.clone() + sentence)) > MAX_TOKENS
            && !current.trim().is_empty()
        {
            out.push(std::mem::take(&mut current));
        }
        current.push_str(sentence);
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// Blocks are the natural chunk boundary in a block editor — a paragraph is an
/// authored semantic unit. Two adjustments: short blocks merge forward so a
/// heading carries what it introduces, and long blocks split at sentence
/// boundaries.
pub fn chunk_blocks(blocks: &[SourceBlock]) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut pending = String::new();

    for block in blocks {
        let t = block.text.trim();
        if t.is_empty() {
            continue;
        }

        if !pending.is_empty() {
            pending.push('\n');
        }
        pending.push_str(t);

        if estimate_tokens(&pending) < MIN_TOKENS {
            continue; // too short — merge forward into the next block
        }

        if estimate_tokens(&pending) > MAX_TOKENS {
            for piece in split_long(&pending) {
                out.push(make(piece.trim().to_string()));
            }
        } else {
            out.push(make(pending.clone()));
        }
        pending.clear();
    }

    // A trailing short block has nothing to merge into — emit it anyway rather
    // than dropping the user's text.
    if !pending.trim().is_empty() {
        out.push(make(pending.trim().to_string()));
    }
    out
}

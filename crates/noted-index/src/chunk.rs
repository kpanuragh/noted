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

/// True for scripts with no whitespace word boundaries, which tokenize at
/// roughly one token per character.
fn is_dense_script(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF |   // Hiragana + Katakana
        0x3400..=0x4DBF |   // CJK Unified Ideographs Extension A
        0x4E00..=0x9FFF |   // CJK Unified Ideographs
        0xAC00..=0xD7AF |   // Hangul Syllables
        0xF900..=0xFAFF     // CJK Compatibility Ideographs
    )
}

/// Rough token count, used only to pick split points — being 30% high costs a
/// slightly smaller chunk, while being low costs a silently truncated embedding.
/// So this deliberately OVER-estimates.
///
/// Three inputs, because one heuristic does not cover them:
///  - whitespace-delimited words (English prose): ~1.3 tokens/word
///  - dense scripts (CJK/Kana/Hangul): ~1 token/char, and `split_whitespace`
///    would otherwise read a whole paragraph as ONE word
///  - a character floor: catches a long URL, base64, or minified code, which is
///    one "word" but many tokens
pub fn estimate_tokens(text: &str) -> i32 {
    let dense = text.chars().filter(|c| is_dense_script(*c)).count();
    let sparse: String = text.chars().filter(|c| !is_dense_script(*c)).collect();
    let words = sparse.split_whitespace().count();

    let by_words = (words as f32 * 1.3) + dense as f32;
    let by_chars = text.chars().count() as f32 / 4.0;

    by_words.max(by_chars).ceil() as i32
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
/// terminators (e.g. a wall of prose with no full stops), and further falls
/// back to a hard character-split for text with no word boundaries either
/// (dense scripts like CJK/Kana/Hangul have neither sentence terminators nor
/// whitespace, so the word-split alone would emit the whole text as "one
/// word" and never actually shrink it).
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
            if words.len() > 1 {
                let per = (MAX_TOKENS as f32 / 1.3).floor() as usize;
                for w in words.chunks(per.max(1)) {
                    out.push(w.join(" "));
                }
            } else {
                // No whitespace boundary either — split on raw characters.
                // Use a conservative budget since dense scripts cost ~1
                // token/char (estimate_tokens' worst case).
                let per_chars = (MAX_TOKENS as f32 * 0.9).floor() as usize;
                let chars: Vec<char> = sentence.chars().collect();
                for piece in chars.chunks(per_chars.max(1)) {
                    out.push(piece.iter().collect());
                }
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

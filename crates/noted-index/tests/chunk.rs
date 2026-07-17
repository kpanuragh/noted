use noted_index::chunk::{chunk_blocks, Chunk, SourceBlock};

fn b(node_type: &str, text: &str) -> SourceBlock {
    SourceBlock { node_type: node_type.into(), text: text.into() }
}

fn long_text(words: usize) -> String {
    std::iter::repeat("word").take(words).collect::<Vec<_>>().join(" ")
}

#[test]
fn a_normal_block_becomes_one_chunk() {
    let blocks = vec![b("paragraph", &long_text(100))];
    let chunks = chunk_blocks(&blocks);
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].text.contains("word"));
}

#[test]
fn a_short_heading_merges_forward_into_the_next_block() {
    // A heading alone is useless to embed — it must carry what it introduces.
    let blocks = vec![b("heading", "Postgres tuning"), b("paragraph", &long_text(100))];
    let chunks = chunk_blocks(&blocks);
    assert_eq!(chunks.len(), 1, "a short block must merge forward, not stand alone");
    assert!(chunks[0].text.contains("Postgres tuning"), "the heading text must survive");
    assert!(chunks[0].text.contains("word"), "the following paragraph must be included");
}

#[test]
fn a_long_block_splits_and_every_piece_is_under_the_ceiling() {
    let blocks = vec![b("paragraph", &format!("{}. {}. {}.", long_text(400), long_text(400), long_text(400)))];
    let chunks = chunk_blocks(&blocks);
    assert!(chunks.len() > 1, "a block over the ceiling must split");
    for c in &chunks {
        assert!(
            c.token_estimate <= noted_index::chunk::MAX_TOKENS,
            "every chunk must be under the ceiling, got {}",
            c.token_estimate
        );
    }
}

#[test]
fn empty_and_whitespace_blocks_produce_no_chunks() {
    let blocks = vec![b("paragraph", ""), b("paragraph", "   \n  ")];
    assert!(chunk_blocks(&blocks).is_empty(), "empty blocks must not be embedded");
}

/// Content addressing is only useful if the hash is a pure function of the text.
#[test]
fn identical_input_produces_identical_hashes() {
    let blocks = vec![b("paragraph", &long_text(100))];
    let a = chunk_blocks(&blocks);
    let b2 = chunk_blocks(&blocks);
    assert_eq!(a[0].content_hash, b2[0].content_hash);
}

#[test]
fn different_text_produces_different_hashes() {
    let x = chunk_blocks(&[b("paragraph", &long_text(100))]);
    let y = chunk_blocks(&[b("paragraph", &long_text(101))]);
    assert_ne!(x[0].content_hash, y[0].content_hash);
}

/// A trailing short block has nothing to merge forward into — it must still be
/// emitted rather than silently dropped.
#[test]
fn a_trailing_short_block_is_not_lost() {
    let blocks = vec![b("paragraph", &long_text(100)), b("paragraph", "short tail")];
    let chunks: Vec<Chunk> = chunk_blocks(&blocks);
    let all: String = chunks.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(" ");
    assert!(all.contains("short tail"), "a trailing short block must not be dropped");
}

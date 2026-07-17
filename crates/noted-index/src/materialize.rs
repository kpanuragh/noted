use noted_db::PgPool;
use uuid::Uuid;

use crate::chunk::{chunk_blocks, SourceBlock};

/// Read a page's blocks, chunk them, and upsert the results.
///
/// Chunking is a pure function, so this is safe to re-run at any time: unchanged
/// text produces the same hashes and `upsert` ignores them. Old chunks are NOT
/// deleted here — a hash orphaned by an edit is garbage, collectable later, and
/// deleting it eagerly would throw away an embedding that a re-edit or another
/// page may want back.
pub async fn rechunk_page(pool: &PgPool, page_id: Uuid) -> Result<usize, sqlx::Error> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT node_type, text FROM blocks WHERE page_id = $1 ORDER BY block_index",
    )
    .bind(page_id)
    .fetch_all(pool)
    .await?;

    let blocks: Vec<SourceBlock> = rows
        .into_iter()
        .map(|(node_type, text)| SourceBlock { node_type, text })
        .collect();

    let chunks = chunk_blocks(&blocks);

    // Order matters: the chunk rows must exist before page_chunks can reference
    // them (FK), and set_page_chunks must run even when `chunks` is empty so a
    // page emptied of text drops its stale links.
    let rows: Vec<(String, String, i32)> = chunks
        .iter()
        .map(|c| (c.content_hash.clone(), c.text.clone(), c.token_estimate))
        .collect();
    noted_db::chunks::upsert(pool, &rows).await?;

    let hashes: Vec<String> = chunks.iter().map(|c| c.content_hash.clone()).collect();
    noted_db::chunks::set_page_chunks(pool, page_id, &hashes).await?;

    Ok(chunks.len())
}

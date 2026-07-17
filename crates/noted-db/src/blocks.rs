use noted_crdt::ProjectedBlock;
use sqlx::PgPool;
use uuid::Uuid;

/// Replace a page's entire projection atomically. Cheaper and far simpler than
/// diffing, because a page's block count is small.
pub async fn replace_for_page(
    pool: &PgPool,
    page_id: Uuid,
    blocks: &[ProjectedBlock],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    sqlx::query("DELETE FROM blocks WHERE page_id = $1")
        .bind(page_id)
        .execute(&mut *tx)
        .await?;

    if !blocks.is_empty() {
        let indices: Vec<i32> = blocks.iter().map(|b| b.index).collect();
        let node_types: Vec<String> = blocks.iter().map(|b| b.node_type.clone()).collect();
        let texts: Vec<String> = blocks.iter().map(|b| b.text.clone()).collect();
        let hashes: Vec<String> = blocks.iter().map(|b| b.content_hash.clone()).collect();

        sqlx::query(
            "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
             SELECT $1, * FROM UNNEST($2::int[], $3::text[], $4::text[], $5::text[])",
        )
        .bind(page_id)
        .bind(&indices)
        .bind(&node_types)
        .bind(&texts)
        .bind(&hashes)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await
}

pub async fn for_page(pool: &PgPool, page_id: Uuid) -> Result<Vec<ProjectedBlock>, sqlx::Error> {
    let rows: Vec<(i32, String, String, String)> = sqlx::query_as(
        "SELECT block_index, node_type, text, content_hash
         FROM blocks WHERE page_id = $1 ORDER BY block_index",
    )
    .bind(page_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(index, node_type, text, content_hash)| ProjectedBlock {
            index,
            node_type,
            text,
            content_hash,
        })
        .collect())
}

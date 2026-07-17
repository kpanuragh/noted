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

    for b in blocks {
        sqlx::query(
            "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(page_id)
        .bind(b.index)
        .bind(&b.node_type)
        .bind(&b.text)
        .bind(&b.content_hash)
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

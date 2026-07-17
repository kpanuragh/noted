use pgvector::Vector;
use sqlx::PgPool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PendingChunk {
    pub content_hash: String,
    pub text: String,
}

/// Insert chunks, ignoring ones already present. Content-addressed, so an
/// existing hash means the text is byte-identical — there is nothing to update.
pub async fn upsert(
    pool: &PgPool,
    rows: &[(String, String, i32)],
) -> Result<u64, sqlx::Error> {
    if rows.is_empty() {
        return Ok(0);
    }
    let hashes: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
    let texts: Vec<String> = rows.iter().map(|r| r.1.clone()).collect();
    let tokens: Vec<i32> = rows.iter().map(|r| r.2).collect();

    let res = sqlx::query(
        "INSERT INTO chunks (content_hash, text, token_estimate)
         SELECT * FROM UNNEST($1::text[], $2::text[], $3::int[])
         ON CONFLICT (content_hash) DO NOTHING",
    )
    .bind(&hashes)
    .bind(&texts)
    .bind(&tokens)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Replace a page's chunk list. Mirrors `blocks::replace_for_page`: the page's
/// chunk set is rewritten wholesale, which is cheap and avoids diffing.
///
/// The `chunks` rows themselves are NOT deleted — they are content-addressed and
/// may be shared with other pages, and an orphan keeps an embedding a re-edit
/// might want back.
pub async fn set_page_chunks(
    pool: &PgPool,
    page_id: uuid::Uuid,
    hashes: &[String],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM page_chunks WHERE page_id = $1")
        .bind(page_id)
        .execute(&mut *tx)
        .await?;

    if !hashes.is_empty() {
        let indices: Vec<i32> = (0..hashes.len() as i32).collect();
        sqlx::query(
            "INSERT INTO page_chunks (page_id, chunk_index, content_hash)
             SELECT $1, * FROM UNNEST($2::int[], $3::text[])",
        )
        .bind(page_id)
        .bind(&indices)
        .bind(hashes)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// THE WORK QUEUE. Not a table — a set difference. Every chunk referenced by a
/// live page that has no embedding for `model_id` yet.
///
/// Note it joins through `page_chunks`, NOT through `blocks`. Chunk hashes and
/// block hashes are different hash spaces and never join: chunking merges short
/// blocks and splits long ones, so a chunk's text is generally not any block's
/// text. `page_chunks` is the only link between a page and its chunks.
///
/// This shape is why the pipeline is crash-safe with no bookkeeping: there is no
/// "in progress" state to leak. A worker killed mid-batch simply leaves those
/// hashes unembedded, and the next poll returns them. It is idempotent and
/// self-healing — a reproject + rechunk immediately shows up as new work.
pub async fn pending(
    pool: &PgPool,
    model_id: &str,
    limit: i64,
) -> Result<Vec<PendingChunk>, sqlx::Error> {
    sqlx::query_as::<_, PendingChunk>(
        "SELECT DISTINCT c.content_hash, c.text
         FROM page_chunks pc
         JOIN chunks c ON c.content_hash = pc.content_hash
         LEFT JOIN embeddings e
           ON e.content_hash = c.content_hash AND e.model_id = $1
         WHERE e.content_hash IS NULL
         LIMIT $2",
    )
    .bind(model_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn store_embedding(
    pool: &PgPool,
    content_hash: &str,
    model_id: &str,
    embedding: &[f32],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO embeddings (content_hash, model_id, embedding)
         VALUES ($1, $2, $3)
         ON CONFLICT (content_hash) DO UPDATE
           SET embedding = EXCLUDED.embedding,
               model_id = EXCLUDED.model_id,
               created_at = now()",
    )
    .bind(content_hash)
    .bind(model_id)
    .bind(Vector::from(embedding.to_vec()))
    .execute(pool)
    .await?;
    Ok(())
}

/// (embedded, total) over LIVE chunks under `model_id`. A real fraction, not an
/// estimate — computable at any moment because the queue is a query.
///
/// Counts through `page_chunks` so orphaned chunks (text that was edited away
/// but whose row we keep) never drag the denominator down. Reaching 100% must
/// mean "everything a user can actually see is indexed".
pub async fn progress(pool: &PgPool, model_id: &str) -> Result<(i64, i64), sqlx::Error> {
    let row: (i64, i64) = sqlx::query_as(
        "SELECT
           count(*) FILTER (WHERE e.content_hash IS NOT NULL) AS embedded,
           count(*)                                            AS total
         FROM (SELECT DISTINCT content_hash FROM page_chunks) pc
         LEFT JOIN embeddings e
           ON e.content_hash = pc.content_hash AND e.model_id = $1",
    )
    .bind(model_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

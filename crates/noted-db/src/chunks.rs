use pgvector::Vector;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PendingChunk {
    pub content_hash: String,
    pub text: String,
}

/// Insert chunks, ignoring ones already present. Content-addressed, so an
/// existing hash means the text is byte-identical — there is nothing to update.
pub async fn upsert(pool: &PgPool, rows: &[(String, String, i32)]) -> Result<u64, sqlx::Error> {
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
/// THE EMBEDDING WORK QUEUE. Not a table — a set difference, mirroring
/// `graph::pending_extraction`.
///
/// `workspace_id: None` drains the whole instance — what the CLI wants.
/// `Some(id)` scopes the queue to chunks referenced by a live page in that
/// one workspace — required so a per-tenant embedding run (or a test on a
/// shared dev database) does not pull in every OTHER workspace's pending
/// chunks too. Mirrors `progress`'s (and `graph::pending_extraction`'s)
/// `$3::uuid IS NULL OR p.workspace_id = $3` scoping exactly, joining through
/// `pages` the same way. Keeps the query string `'static` (bind, never
/// interpolate).
pub async fn pending(
    pool: &PgPool,
    model_id: &str,
    workspace_id: Option<Uuid>,
    limit: i64,
) -> Result<Vec<PendingChunk>, sqlx::Error> {
    // ORDER BY is for DETERMINISM, not starvation: `LIMIT` without it lets
    // Postgres return any rows it likes, which made tests flaky. It is emphatically
    // NOT a fairness mechanism — none is needed. Every batch that succeeds inserts
    // into `embeddings`, so the set difference this query computes strictly shrinks;
    // progress is monotonic whatever order the rows come back in.
    sqlx::query_as::<_, PendingChunk>(
        "SELECT DISTINCT c.content_hash, c.text
         FROM page_chunks pc
         JOIN pages p ON p.id = pc.page_id
         JOIN chunks c ON c.content_hash = pc.content_hash
         LEFT JOIN embeddings e
           ON e.content_hash = c.content_hash AND e.model_id = $1
         WHERE e.content_hash IS NULL
           AND ($3::uuid IS NULL OR p.workspace_id = $3)
         ORDER BY c.content_hash
         LIMIT $2",
    )
    .bind(model_id)
    .bind(limit)
    .bind(workspace_id)
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
         ON CONFLICT (content_hash, model_id) DO UPDATE
           SET embedding = EXCLUDED.embedding",
    )
    .bind(content_hash)
    .bind(model_id)
    .bind(Vector::from(embedding.to_vec()))
    .execute(pool)
    .await?;
    Ok(())
}

/// Store a whole batch of embeddings in ONE round trip.
///
/// The worker embeds `BATCH_SIZE` chunks at a time; storing them with
/// `store_embedding` in a loop is N+1 round trips per batch — the exact pattern
/// `blocks::replace_for_page` already replaced with a single `UNNEST` insert.
/// Same semantics as `store_embedding`, just set-at-a-time.
pub async fn store_embeddings_batch(
    pool: &PgPool,
    model_id: &str,
    rows: &[(String, Vec<f32>)],
) -> Result<(), sqlx::Error> {
    if rows.is_empty() {
        return Ok(());
    }
    let hashes: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
    let vectors: Vec<Vector> = rows.iter().map(|r| Vector::from(r.1.clone())).collect();

    sqlx::query(
        "INSERT INTO embeddings (content_hash, model_id, embedding)
         SELECT h, $2, v FROM UNNEST($1::text[], $3::vector[]) AS t(h, v)
         ON CONFLICT (content_hash, model_id) DO UPDATE
           SET embedding = EXCLUDED.embedding",
    )
    .bind(&hashes)
    .bind(model_id)
    .bind(&vectors)
    .execute(pool)
    .await?;
    Ok(())
}

/// (embedded, total) over LIVE chunks under `model_id`.
///
/// `workspace_id: None` counts the whole instance — correct for the indexing CLI,
/// which drains everything. `Some(id)` scopes to one workspace: required for any
/// user-facing "N% indexed" figure, because a global number would expose one
/// tenant's backfill volume to another.
///
/// Counts through `page_chunks` so orphaned chunks (text that was edited away
/// but whose row we keep) never drag the denominator down. Reaching 100% must
/// mean "everything a user can actually see is indexed".
pub async fn progress(
    pool: &PgPool,
    model_id: &str,
    workspace_id: Option<uuid::Uuid>,
) -> Result<(i64, i64), sqlx::Error> {
    let row: (i64, i64) = sqlx::query_as(
        "SELECT
           count(*) FILTER (WHERE e.content_hash IS NOT NULL) AS embedded,
           count(*)                                            AS total
         FROM (
             SELECT DISTINCT pc.content_hash
             FROM page_chunks pc
             JOIN pages p ON p.id = pc.page_id
             WHERE $2::uuid IS NULL OR p.workspace_id = $2
         ) pc
         LEFT JOIN embeddings e
           ON e.content_hash = pc.content_hash AND e.model_id = $1",
    )
    .bind(model_id)
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

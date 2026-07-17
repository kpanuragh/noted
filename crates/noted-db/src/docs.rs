use sqlx::PgPool;
use uuid::Uuid;

/// Compact once the log exceeds this many updates. Chosen so replay stays
/// cheap on page open without compacting on every keystroke.
pub const COMPACT_THRESHOLD: i64 = 100;

pub async fn append(pool: &PgPool, page_id: Uuid, update: &[u8]) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Claim the next seq under a row lock so concurrent appends cannot collide.
    let seq: i64 = sqlx::query_scalar(
        "INSERT INTO doc_seq (page_id, next) VALUES ($1, 1)
         ON CONFLICT (page_id) DO UPDATE SET next = doc_seq.next + 1
         RETURNING next - 1",
    )
    .bind(page_id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query("INSERT INTO doc_updates (page_id, seq, update) VALUES ($1, $2, $3)")
        .bind(page_id)
        .bind(seq)
        .bind(update)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(seq)
}

pub async fn load(pool: &PgPool, page_id: Uuid) -> Result<Vec<Vec<u8>>, sqlx::Error> {
    sqlx::query_scalar("SELECT update FROM doc_updates WHERE page_id = $1 ORDER BY seq")
        .bind(page_id)
        .fetch_all(pool)
        .await
}

pub async fn update_count(pool: &PgPool, page_id: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM doc_updates WHERE page_id = $1")
        .bind(page_id)
        .fetch_one(pool)
        .await
}

/// Replace the entire log with a single snapshot, atomically. `snapshot` must
/// encode the full document state (`NotedDoc::encode_full`).
pub async fn compact(pool: &PgPool, page_id: Uuid, snapshot: &[u8]) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Lock the sequence row first so no append interleaves with the delete.
    sqlx::query("SELECT next FROM doc_seq WHERE page_id = $1 FOR UPDATE")
        .bind(page_id)
        .fetch_optional(&mut *tx)
        .await?;

    sqlx::query("DELETE FROM doc_updates WHERE page_id = $1")
        .bind(page_id)
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        "INSERT INTO doc_seq (page_id, next) VALUES ($1, 1)
         ON CONFLICT (page_id) DO UPDATE SET next = 1",
    )
    .bind(page_id)
    .execute(&mut *tx)
    .await?;

    sqlx::query("INSERT INTO doc_updates (page_id, seq, update) VALUES ($1, 0, $2)")
        .bind(page_id)
        .bind(snapshot)
        .execute(&mut *tx)
        .await?;

    tx.commit().await
}

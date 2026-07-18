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

    // Content edits are what `pages.updated_at` is FOR, and until now only
    // `pages::rename` ever wrote it — so a "recently edited" view showed rename
    // time and a user could type all day without their page moving.
    // `doc_updates` cannot supply recency instead: it has no timestamp of its
    // own, and `compact` collapses the whole log into one row anyway.
    //
    // IN THIS TRANSACTION, DELIBERATELY. The append is the edit; if the bump
    // could commit separately the two would disagree in both directions — a
    // rolled-back append leaving `updated_at` claiming an edit that is not in
    // the log, or a committed append the dashboard never surfaces. Same
    // transaction makes those states unrepresentable.
    //
    // LAST in the transaction, and this ordering matters: `append` now takes a
    // `doc_seq` row lock and then a `pages` row lock, always in that order.
    // Nothing in the codebase takes them the other way round (`pages::rename`
    // touches only `pages`; `compact` only `doc_seq` + `doc_updates`), so no
    // deadlock cycle exists — but a future writer that locks `pages` first and
    // then appends would create one.
    //
    // Unconditional, i.e. one row version per append. See the note on write
    // amplification in `pages::recent`.
    sqlx::query("UPDATE pages SET updated_at = now() WHERE id = $1")
        .bind(page_id)
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
    //
    // Guarantee the doc_seq row exists so the FOR UPDATE below ALWAYS takes a
    // lock. Without this, compacting a page with no prior appends would find no
    // row, take no lock, and race a concurrent first append into oblivion.
    sqlx::query(
        "INSERT INTO doc_seq (page_id, next) VALUES ($1, 0) ON CONFLICT (page_id) DO NOTHING",
    )
    .bind(page_id)
    .execute(&mut *tx)
    .await?;

    // Lock-only: the value is unused. This serialises against append()'s
    // upsert, which takes a conflicting row lock on the same doc_seq row.
    sqlx::query("SELECT next FROM doc_seq WHERE page_id = $1 FOR UPDATE")
        .bind(page_id)
        .fetch_one(&mut *tx)
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

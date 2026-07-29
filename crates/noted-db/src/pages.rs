use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Page {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const COLS: &str = "id, workspace_id, parent_id, title, created_at, updated_at";

pub async fn create(
    pool: &PgPool,
    workspace_id: Uuid,
    parent_id: Option<Uuid>,
    title: &str,
) -> Result<Page, sqlx::Error> {
    // SAFETY: only COLS (a &'static str const) is interpolated; every runtime
    // value below is parameter-bound. Never wrap a string containing user input.
    sqlx::query_as::<_, Page>(sqlx::AssertSqlSafe(format!(
        "INSERT INTO pages (workspace_id, parent_id, title)
         VALUES ($1, $2, $3) RETURNING {COLS}"
    )))
    .bind(workspace_id)
    .bind(parent_id)
    .bind(title)
    .fetch_one(pool)
    .await
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<Page>, sqlx::Error> {
    // SAFETY: only COLS (a &'static str const) is interpolated; every runtime
    // value below is parameter-bound. Never wrap a string containing user input.
    sqlx::query_as::<_, Page>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM pages WHERE id = $1 AND archived_at IS NULL"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn children(
    pool: &PgPool,
    workspace_id: Uuid,
    parent_id: Option<Uuid>,
) -> Result<Vec<Page>, sqlx::Error> {
    // `IS NOT DISTINCT FROM` so a NULL parent_id matches root pages.
    // SAFETY: only COLS (a &'static str const) is interpolated; every runtime
    // value below is parameter-bound. Never wrap a string containing user input.
    sqlx::query_as::<_, Page>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM pages
         WHERE workspace_id = $1
           AND parent_id IS NOT DISTINCT FROM $2
           AND archived_at IS NULL
         ORDER BY created_at"
    )))
    .bind(workspace_id)
    .bind(parent_id)
    .fetch_all(pool)
    .await
}

/// Every non-archived page in the instance. Used by the indexer to materialise
/// chunks for a corpus that predates the pipeline.
///
/// Ordered by `(created_at, id)` so a backfill walks the corpus in a stable
/// order across runs — `created_at` alone is not unique, hence the `id`
/// tiebreak.
pub async fn all_page_ids(pool: &PgPool) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM pages WHERE archived_at IS NULL ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await
}

/// Hard ceiling on how many rows `recent` will return, whatever the caller asks
/// for. An uncapped, caller-supplied `LIMIT` is a trivial denial of service: one
/// request could ask for a tenant's entire page table. The cap lives HERE rather
/// than only in the HTTP handler so every caller inherits it — a second entry
/// point that forgot to clamp would otherwise reopen the hole.
pub const MAX_RECENT_LIMIT: i64 = 50;

/// Most recently EDITED live pages in a workspace, newest first.
///
/// "Recently edited" means `pages.updated_at`, which `docs::append` now bumps in
/// the same transaction as every persisted document update (it previously moved
/// only on rename, so this list would have shown rename time). `doc_updates`
/// cannot be used for recency instead: it carries no timestamp, and `compact`
/// merges the whole log into one row.
///
/// "Live" is `archived_at IS NULL` — the SAME definition shared by
/// `chunks::pending`, `chunks::progress`, `graph::pending_extraction`,
/// `graph::extraction_progress`, `all_page_ids`, `community`'s
/// `clusterable_edges` CTE, `stats::workspace_stats` and migration 0010's
/// partial index. A differing notion of live would be a bug, not a feature.
/// No count is given on purpose: this list has grown three times and a stale
/// number reads as a stale invariant.
///
/// `limit` is clamped to `1..=MAX_RECENT_LIMIT`. A zero or negative limit is
/// caller error, not a request for everything, and is treated as 1.
///
/// ON WRITE AMPLIFICATION (deliberately not debounced, recorded for later):
/// because `docs::append` bumps `updated_at` unconditionally, an actively-typed
/// page writes a new `pages` row version per persisted update, and since
/// `updated_at` is indexed by `pages_workspace_updated_idx` those updates cannot
/// be HOT — each one adds an entry to every index on `pages`, the GIN trigram
/// index on `title` included. That is real, but small: sync appends run at
/// human typing rates, and the log itself already pays a far heavier
/// `DELETE`+`INSERT` of the entire document every `COMPACT_THRESHOLD` updates.
/// Autovacuum handles this volume comfortably, so a debounce would be
/// complexity bought against a cost nobody has measured.
/// IF it ever needs one, the cheap shape is a threshold in the same statement —
/// `... WHERE id = $1 AND updated_at < now() - interval '30 seconds'` — which
/// caps the write rate per page while keeping the bump inside the append
/// transaction (a debounce in `sync.rs` would not; it would put the bump on a
/// different commit from the edit it describes, which is exactly what makes the
/// two able to disagree). Note that such a threshold would also make the
/// single-append test in `tests/docs.rs` unable to fail, so it must arrive with
/// a test written against the threshold, not around it.
pub async fn recent(
    pool: &PgPool,
    workspace_id: Uuid,
    limit: i64,
) -> Result<Vec<Page>, sqlx::Error> {
    let limit = limit.clamp(1, MAX_RECENT_LIMIT);
    // `id DESC` is a determinism tiebreak, not a ranking choice: `updated_at`
    // is not unique, and a bare LIMIT over a non-unique sort key lets Postgres
    // return any of the tied rows. It matches `pages_workspace_updated_idx`'s
    // trailing column so the index still supplies the whole ordering.
    // SAFETY: only COLS (a &'static str const) is interpolated; every runtime
    // value below is parameter-bound. Never wrap a string containing user input.
    sqlx::query_as::<_, Page>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM pages
         WHERE workspace_id = $1
           AND archived_at IS NULL
         ORDER BY updated_at DESC, id DESC
         LIMIT $2"
    )))
    .bind(workspace_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Archive a page — what "delete" means here.
///
/// Soft, and deliberately so. `archived_at` is not a new idea: every read query
/// in this module already filters on it, and `graph::reap_graph` already treats
/// an archived page's entities and edges as dead and sweeps them. A hard DELETE
/// would have to cascade through chunks, embeddings, edges and communities — or
/// leave them orphaned — for no gain, because what a person means by "delete
/// this note" is that it stops appearing, not that its bytes are unrecoverable.
///
/// `AND archived_at IS NULL` makes this idempotent: archiving twice reports no
/// change rather than bumping the timestamp, so a double-click or a retried
/// request cannot quietly rewrite when the page was deleted.
///
/// Returns `Ok(true)` if a live page was archived, `Ok(false)` if there was no
/// live page with that id — which lets the caller answer 404 rather than 500.
pub async fn archive(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE pages SET archived_at = now(), updated_at = now()
         WHERE id = $1 AND archived_at IS NULL",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Returns `Ok(true)` if a page was renamed, `Ok(false)` if no such page exists.
pub async fn rename(pool: &PgPool, id: Uuid, title: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("UPDATE pages SET title = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(title)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

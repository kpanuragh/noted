//! Workspace-level counters for the dashboard.
//!
//! Every figure here is scoped to ONE workspace. That is not decoration: these
//! are the numbers a tenant sees, and a count that quietly spans the instance
//! would expose another tenant's corpus volume while looking perfectly healthy.

use sqlx::PgPool;
use uuid::Uuid;

/// Dashboard counters for a single workspace.
///
/// Definitions, all of which are load-bearing:
///
/// * `pages` — LIVE (non-archived) pages. Archiving is this product's delete.
/// * `chunks_indexed` — DISTINCT chunks referenced by a live page in this
///   workspace that have an embedding under the model in question. This is
///   `chunks::progress`'s "embedded" figure, reused rather than reimplemented.
/// * `entities` / `edges` — rows carrying this `workspace_id`. Both tables
///   carry one: `entities` from the start (M2a), `edges` since migration 0007,
///   which denormalised it precisely so per-workspace graph queries need no
///   join through `entities`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceStats {
    pub pages: i64,
    pub chunks_indexed: i64,
    pub entities: i64,
    pub edges: i64,
}

/// Dashboard counters for `workspace_id`, with `chunks_indexed` measured against
/// `model_id`.
///
/// WHY `model_id` IS A PARAMETER AND NEVER A LITERAL: `embeddings` is keyed
/// `(content_hash, model_id)` and several models' vectors coexist by design
/// (M1b), so "is this chunk indexed" is only answerable per model. Callers must
/// pass the same model the read path searches with — `AppState::embedder`'s
/// `model_id()`, exactly as `routes::search` does. A hardcoded literal here
/// would drift from the write path the moment the model changed and report a
/// workspace as indexed that search returns nothing for. That precise bug
/// already happened once in this codebase, in `search.rs`.
///
/// `chunks_indexed` DELEGATES to [`crate::chunks::progress`] rather than writing
/// its own count. Five queries in this tree already agree on what "live" means
/// (`chunks::pending`, `chunks::progress`, `graph::pending_extraction`,
/// `graph::extraction_progress`, `pages::all_page_ids`) and a sixth that
/// disagreed would be a bug. Delegating makes divergence impossible instead of
/// merely unlikely — including the details that are easy to get subtly wrong:
/// counting DISTINCT content hashes (a chunk on two pages is one chunk), and
/// counting through `page_chunks` so orphaned chunk rows never inflate it.
pub async fn workspace_stats(
    pool: &PgPool,
    workspace_id: Uuid,
    model_id: &str,
) -> Result<WorkspaceStats, sqlx::Error> {
    let (embedded, _total) = crate::chunks::progress(pool, model_id, Some(workspace_id)).await?;

    // The three plain counts share one round trip. They are independent
    // aggregates over different tables, so scalar subqueries are the natural
    // shape — no join, no risk of one table's row count multiplying another's.
    let (pages, entities, edges): (i64, i64, i64) = sqlx::query_as(
        "SELECT
           (SELECT count(*) FROM pages
             WHERE workspace_id = $1 AND archived_at IS NULL),
           (SELECT count(*) FROM entities WHERE workspace_id = $1),
           (SELECT count(*) FROM edges    WHERE workspace_id = $1)",
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;

    Ok(WorkspaceStats {
        pages,
        chunks_indexed: embedded,
        entities,
        edges,
    })
}

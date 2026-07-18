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
/// * `edges` — edges of this workspace whose PROVENANCE IS STILL LIVE, i.e. the
///   `clusterable_edges` set, spliced from `community`'s macro rather than
///   restated here.
/// * `entities` — entities of this workspace named by at least one such edge.
///
/// EVERY FIGURE HERE IS LIVE-SCOPED, and that uniformity was a correction.
/// `entities`/`edges` were once plain row counts carrying this `workspace_id`,
/// which made the struct self-contradictory in a way a user could see: archiving
/// every page reported no pages, no indexed chunks, and a full knowledge graph.
/// It also disagreed with the product — the clusterer, and therefore every
/// community, summary and graph view, has always counted an edge only while a
/// live page evidences it. Same rule `chunks_indexed` was fixed under: a number
/// that can contradict the UI it feeds is a bug in the number.
///
/// CONSEQUENCE, deliberate: an entity with no live edge does not count, so a
/// workspace mid-extraction can show entities climbing behind edges. That is
/// what `clusterable_graph` already reports — its node query restricts to
/// endpoints of live edges — so an edgeless entity is invisible everywhere else
/// in the product too. Counting it here would be the only place it appeared.
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
/// its own count. Every query in this tree that has an opinion on what "live"
/// means already agrees on `archived_at IS NULL` — `chunks::pending`,
/// `chunks::progress`, `graph::pending_extraction`, `graph::extraction_progress`,
/// `pages::all_page_ids`, `pages::recent`, `community`'s `clusterable_edges` CTE,
/// migration 0010's partial index, and the `pages` count below — and one that
/// disagreed would be a bug. That list is DELIBERATELY not given a count: it has
/// grown three times already and a stale number reads as a stale invariant.
/// Delegating makes divergence impossible instead of
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
    //
    // `entities` and `edges` are LIVE counts, spliced from the very same
    // `clusterable_edges` definition the clusterer uses — see the note on the
    // struct field.
    let (pages, entities, edges): (i64, i64, i64) = sqlx::query_as(concat!(
        "WITH ",
        crate::community::clusterable_edges_cte!(),
        " SELECT
           (SELECT count(*) FROM pages
             WHERE workspace_id = $1 AND archived_at IS NULL),
           (SELECT count(*) FROM entities en
             WHERE en.workspace_id = $1
               AND EXISTS (SELECT 1 FROM clusterable_edges ce
                            WHERE ce.source_entity = en.id
                               OR ce.target_entity = en.id)),
           (SELECT count(*) FROM clusterable_edges)"
    ))
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

use axum::Json;
use axum::extract::{Path, State};
use noted_db::stats::WorkspaceStats;
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

/// `GET /api/workspaces/{workspace_id}/stats`
///
/// Dashboard counters for one workspace: live pages, indexed chunks, graph
/// entities and graph edges. Every figure is scoped to `workspace_id` in SQL,
/// never filtered after the fact.
///
/// The model the chunk count is measured against comes from
/// `st.embedder.model_id()` — the SAME source `routes::search` uses for the read
/// path and `noted_index`'s worker uses for the write path. A literal here would
/// drift the moment the model changed and report chunks as indexed that search
/// could never retrieve; that exact bug has already been fixed once in this
/// codebase and the rule is not to reintroduce it.
///
/// An unknown `workspace_id` returns all-zeroes rather than `404`. There is no
/// authentication yet (M4), so probing this endpoint must not be a way to
/// enumerate which workspace ids exist — and "a workspace with nothing in it"
/// and "no such workspace" are genuinely the same answer to every question this
/// endpoint asks. A malformed uuid still fails at the `Path` extractor with
/// `400`.
pub async fn stats(
    State(st): State<AppState>,
    Path(workspace_id): Path<Uuid>,
) -> Result<Json<WorkspaceStats>, AppError> {
    let stats =
        noted_db::stats::workspace_stats(&st.pool, workspace_id, st.embedder.model_id()).await?;
    Ok(Json(stats))
}

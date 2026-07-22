use axum::Json;
use axum::extract::{Extension, Path, State};
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
    crate::membership::MemberWorkspacePath(workspace_id): crate::membership::MemberWorkspacePath,
) -> Result<Json<WorkspaceStats>, AppError> {
    let stats =
        noted_db::stats::workspace_stats(&st.pool, workspace_id, st.embedder.model_id()).await?;
    Ok(Json(stats))
}

/// `GET /api/workspaces` — the workspaces this user belongs to.
///
/// The switcher's data source, and the app's bootstrap: the web client no
/// longer has a hardcoded workspace id, it asks which ones are its own. An
/// empty list is not an error — it means every workspace was left or deleted,
/// and the UI offers to make one.
pub async fn mine(
    State(st): State<AppState>,
    Extension(user): Extension<noted_db::users::User>,
) -> Result<Json<Vec<noted_db::workspaces::Workspace>>, AppError> {
    Ok(Json(
        noted_db::workspaces::for_user(&st.pool, user.id).await?,
    ))
}

#[derive(serde::Deserialize)]
pub struct CreateWorkspaceBody {
    pub name: String,
}

/// `POST /api/workspaces`
pub async fn create(
    State(st): State<AppState>,
    Extension(user): Extension<noted_db::users::User>,
    Json(body): Json<CreateWorkspaceBody>,
) -> Result<(axum::http::StatusCode, Json<noted_db::workspaces::Workspace>), AppError> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("a workspace needs a name".into()));
    }
    let ws = noted_db::workspaces::create(&st.pool, name, user.id).await?;
    Ok((axum::http::StatusCode::CREATED, Json(ws)))
}

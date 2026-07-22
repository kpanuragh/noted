use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use noted_crdt::NotedDoc;
use noted_db::pages::{self, Page};
use noted_db::{blocks, docs};
use uuid::Uuid;

use crate::error::AppError;
use crate::membership::{MemberPage, MemberWorkspace};
use crate::state::AppState;

#[derive(serde::Deserialize)]
pub struct CreateBody {
    pub workspace_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub title: Option<String>,
}

/// The ONE handler that cannot use `MemberWorkspace`: its workspace id arrives
/// in the JSON body, not the query string, and a body extractor must run last
/// (it consumes the request). So the check is explicit here — and it is the
/// only place in the codebase where "remembering to check" is load-bearing,
/// which is why `a_member_of_no_workspace_cannot_create_a_page_in_one` exists.
pub async fn create(
    State(st): State<AppState>,
    Extension(user): Extension<noted_db::users::User>,
    Json(body): Json<CreateBody>,
) -> Result<(StatusCode, Json<Page>), AppError> {
    if !noted_db::workspaces::is_member(&st.pool, body.workspace_id, user.id).await? {
        return Err(AppError::Forbidden);
    }
    let title = body.title.as_deref().unwrap_or("Untitled");
    let page = pages::create(&st.pool, body.workspace_id, body.parent_id, title)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_foreign_key_violation() => {
                AppError::InvalidReference
            }
            _ => AppError::Db(e),
        })?;
    Ok((StatusCode::CREATED, Json(page)))
}

pub async fn get(
    State(st): State<AppState>,
    MemberPage(id): MemberPage,
) -> Result<Json<Page>, AppError> {
    pages::get(&st.pool, id)
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
}

#[derive(serde::Deserialize)]
pub struct ListQuery {
    pub workspace_id: Uuid,
    pub parent_id: Option<Uuid>,
}

pub async fn list(
    State(st): State<AppState>,
    MemberWorkspace(workspace_id): MemberWorkspace,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<Page>>, AppError> {
    Ok(Json(
        pages::children(&st.pool, workspace_id, q.parent_id).await?,
    ))
}

/// Rows returned by `/api/pages/recent` when the caller does not ask for a
/// number. `pages::recent` clamps whatever it is given to
/// `pages::MAX_RECENT_LIMIT`, so this default cannot be raised past the cap by
/// a query string.
const DEFAULT_RECENT_LIMIT: i64 = 10;

#[derive(serde::Deserialize)]
pub struct RecentQuery {
    pub workspace_id: Uuid,
    pub limit: Option<i64>,
}

/// `GET /api/pages/recent?workspace_id=&limit=`
///
/// The dashboard's "recently edited" list: live pages ordered by
/// `updated_at DESC`, which since `docs::append` bumps it now genuinely means
/// last EDIT rather than last rename.
///
/// A missing or malformed `workspace_id` never reaches this handler — axum's
/// `Query` extractor rejects it with `400` first, the same mechanism `list` and
/// `search::quick_find` already rely on.
///
/// `limit` is NOT trusted. The clamp lives in `pages::recent` rather than here
/// so that every caller inherits it, but it bears repeating why it exists at
/// all: an uncapped caller-supplied `LIMIT` lets one request ask for a tenant's
/// entire page table.
///
/// Registered BEFORE `/api/pages/{id}` is not what makes this reachable —
/// axum's router matches a static segment in preference to a dynamic one
/// regardless of declaration order — but `recent` is not a UUID, so if that
/// preference ever changed this route would start returning `400` from the
/// `{id}` extractor. `recent_is_not_shadowed_by_the_page_id_route` pins it.
pub async fn recent(
    State(st): State<AppState>,
    MemberWorkspace(workspace_id): MemberWorkspace,
    Query(q): Query<RecentQuery>,
) -> Result<Json<Vec<Page>>, AppError> {
    let limit = q.limit.unwrap_or(DEFAULT_RECENT_LIMIT);
    Ok(Json(pages::recent(&st.pool, q.workspace_id, limit).await?))
}

#[derive(serde::Deserialize)]
pub struct RenameBody {
    pub title: String,
}

pub async fn rename(
    State(st): State<AppState>,
    MemberPage(id): MemberPage,
    Json(body): Json<RenameBody>,
) -> Result<StatusCode, AppError> {
    let renamed = pages::rename(&st.pool, id, &body.title).await?;
    if renamed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

#[derive(serde::Serialize)]
pub struct ReprojectResponse {
    pub blocks: usize,
}

/// Rebuild a page's `blocks` projection from `doc_updates`, the source of truth.
///
/// `blocks` is a derived projection, and the session writes it on a debounce
/// with failures logged and ignored — so a crash or a failed write can leave it
/// stale with nothing to repair it, while M1b/M1c index off it. This endpoint is
/// that repair path, and the exercise proving the projection really is
/// reconstructible from the log alone.
///
/// Deliberately rebuilt from the log rather than from any live in-memory hub:
/// reading the log is what actually tests the invariant. If a session is editing
/// the page concurrently its own next projection simply overwrites this one with
/// newer state, which is the correct outcome either way.
pub async fn reproject(
    State(st): State<AppState>,
    MemberPage(id): MemberPage,
) -> Result<Json<ReprojectResponse>, AppError> {
    if pages::get(&st.pool, id).await?.is_none() {
        return Err(AppError::NotFound);
    }

    let updates = docs::load(&st.pool, id).await?;
    let doc = NotedDoc::from_updates(&updates).map_err(|e| {
        tracing::error!(error = %e, page_id = %id, "cannot reproject: corrupt doc log");
        AppError::CorruptDoc
    })?;

    let projected = doc.project();
    blocks::replace_for_page(&st.pool, id, &projected).await?;

    // Rebuilding blocks without refreshing chunks would leave the index stale
    // right after the endpoint whose entire purpose is repairing staleness.
    if let Err(e) = noted_index::materialize::rechunk_page(&st.pool, id).await {
        tracing::warn!(error = %e, page_id = %id, "rechunk failed");
    }

    Ok(Json(ReprojectResponse {
        blocks: projected.len(),
    }))
}

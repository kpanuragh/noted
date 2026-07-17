use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use noted_crdt::NotedDoc;
use noted_db::pages::{self, Page};
use noted_db::{blocks, docs};
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

#[derive(serde::Deserialize)]
pub struct CreateBody {
    pub workspace_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub title: Option<String>,
}

pub async fn create(
    State(st): State<AppState>,
    Json(body): Json<CreateBody>,
) -> Result<(StatusCode, Json<Page>), AppError> {
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

pub async fn get(State(st): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<Page>, AppError> {
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
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<Page>>, AppError> {
    Ok(Json(
        pages::children(&st.pool, q.workspace_id, q.parent_id).await?,
    ))
}

#[derive(serde::Deserialize)]
pub struct RenameBody {
    pub title: String,
}

pub async fn rename(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
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
    Path(id): Path<Uuid>,
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

    Ok(Json(ReprojectResponse {
        blocks: projected.len(),
    }))
}

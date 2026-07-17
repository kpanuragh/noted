use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use noted_db::pages::{self, Page};
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

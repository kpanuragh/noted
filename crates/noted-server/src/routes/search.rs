use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use noted_db::search::{QuickHit, RelatedHit, SearchHit};
use uuid::Uuid;

use crate::error::AppError;
use crate::membership::{MemberPage, MemberWorkspace};
use crate::state::AppState;

/// Arbitrary, generous result cap. None of the three endpoints take a
/// caller-supplied limit yet — that's a future knob, not a correctness
/// requirement of this task.
const DEFAULT_LIMIT: i64 = 20;

#[derive(Debug, serde::Deserialize)]
pub struct QuickFindQuery {
    pub workspace_id: Uuid,
    #[serde(default)]
    pub q: String,
}

/// `GET /api/quickfind?workspace_id=&q=`
///
/// A missing or malformed `workspace_id` never reaches this function: axum's
/// `Query` extractor rejects with `400 Bad Request` before the handler runs
/// (`QueryRejection`'s `IntoResponse` — the same mechanism `pages::list`
/// already relies on for its own required `workspace_id`).
pub async fn quick_find(
    State(st): State<AppState>,
    MemberWorkspace(workspace_id): MemberWorkspace,
    Extension(user): Extension<noted_db::users::User>,
    Query(q): Query<QuickFindQuery>,
) -> Result<Json<Vec<QuickHit>>, AppError> {
    let hits =
        noted_db::search::quick_find(&st.pool, workspace_id, user.id, &q.q, DEFAULT_LIMIT).await?;
    Ok(Json(hits))
}

#[derive(Debug, serde::Deserialize)]
pub struct SearchQuery {
    pub workspace_id: Uuid,
    #[serde(default)]
    pub q: String,
}

/// `GET /api/search?workspace_id=&q=`
///
/// Embeds `q` through the state's shared embedder (loaded once at startup —
/// see `AppState::embedder`) and fuses it with the lexical arm via
/// `noted_db::search::hybrid`. An embedding failure is logged with detail and
/// surfaced to the client as a bare `500` (`AppError::Embed`) — never the
/// underlying ONNX/model error text.
///
/// A blank (or whitespace-only) query short-circuits before the embed call —
/// `hybrid` would trim and return empty anyway, but not before paying for a
/// multi-second model call. `quick_find` already short-circuits the same way.
pub async fn search(
    State(st): State<AppState>,
    MemberWorkspace(workspace_id): MemberWorkspace,
    Extension(user): Extension<noted_db::users::User>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Vec<SearchHit>>, AppError> {
    if q.q.trim().is_empty() {
        return Ok(Json(Vec::new()));
    }

    let mut vectors = st.embedder.embed(&[q.q.clone()]).await.map_err(|e| {
        tracing::error!(error = %e, "failed to embed search query");
        AppError::Embed
    })?;
    let q_vec = vectors.pop().ok_or(AppError::Embed)?;

    let hits = noted_db::search::hybrid(
        &st.pool,
        workspace_id,
        user.id,
        &q.q,
        &q_vec,
        st.embedder.model_id(),
        DEFAULT_LIMIT,
    )
    .await?;
    Ok(Json(hits))
}

/// `GET /api/pages/{id}/related`
///
/// Needs no embedding call: `related_pages` compares the page's own stored
/// chunk embeddings against every other chunk's, so this endpoint is cheap
/// even with the real model wired in. An unknown page must 404, not return an
/// empty list — `related_pages`'s SQL alone can't distinguish "page has no
/// neighbours" from "page doesn't exist" (both yield zero rows), so existence
/// is checked explicitly first, the same pattern `pages::reproject` uses.
pub async fn related(
    State(st): State<AppState>,
    MemberPage(id): MemberPage,
) -> Result<Json<Vec<RelatedHit>>, AppError> {
    if noted_db::pages::get(&st.pool, id).await?.is_none() {
        return Err(AppError::NotFound);
    }

    let hits =
        noted_db::search::related_pages(&st.pool, id, st.embedder.model_id(), DEFAULT_LIMIT)
            .await?;
    Ok(Json(hits))
}

//! The Ask surface — M2c's HTTP face.
//!
//! Two modes, deliberately separate endpoints rather than one endpoint with a
//! `mode` parameter: they answer different KINDS of question, have different
//! costs (global search is one model call per theme plus a reduce), and return
//! genuinely different shapes. Collapsing them would mean a response type that
//! is half-empty whichever way it is called.
use axum::Json;
use axum::extract::{Query, State};
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

/// Evidence budget for a local search. Not caller-supplied: `local_search`
/// clamps its own `k`, but an endpoint that let the caller pick would still be
/// handing a stranger a knob on how much work the traversal does.
const LOCAL_K: i64 = 8;

#[derive(Debug, serde::Deserialize)]
pub struct AskQuery {
    pub workspace_id: Uuid,
    #[serde(default)]
    pub q: String,
}

/// `GET /api/ask/local?workspace_id=&q=`
///
/// Entity-anchored: hybrid search seeds the graph, the graph reaches what the
/// question is connected to, and every citation says which of those it was.
pub async fn local(
    State(st): State<AppState>,
    Query(q): Query<AskQuery>,
) -> Result<Json<noted_index::graph_search::LocalAnswer>, AppError> {
    let question = q.q.trim();
    if question.is_empty() {
        return Err(AppError::BadRequest(
            "ask requires a non-empty question".into(),
        ));
    }

    // Embedded through the state's shared embedder, loaded once at startup —
    // the same path `search::search` uses. Constructing one per request would
    // reload ~417MB of ONNX weights on every question.
    let mut vectors = st
        .embedder
        .embed(&[question.to_string()])
        .await
        .map_err(|e| AppError::AskFailed(format!("embedding the question failed: {e}")))?;
    let q_vec = vectors
        .pop()
        .ok_or_else(|| AppError::AskFailed("embedder returned no vector".into()))?;

    let answer = noted_index::graph_search::local_search(
        &st.pool,
        q.workspace_id,
        question,
        &q_vec,
        st.embedder.model_id(),
        st.answerer.as_ref(),
        LOCAL_K,
    )
    .await
    .map_err(|e| AppError::AskFailed(format!("local search failed: {e}")))?;

    Ok(Json(answer))
}

/// `GET /api/ask/global?workspace_id=&q=`
///
/// Theme-anchored: map over the workspace's community summaries and reduce.
/// Answers the question top-k retrieval structurally cannot — "what have I been
/// thinking about?" — and reports how many themes it could NOT consult.
pub async fn global(
    State(st): State<AppState>,
    Query(q): Query<AskQuery>,
) -> Result<Json<noted_index::global_search::GlobalAnswer>, AppError> {
    let question = q.q.trim();
    if question.is_empty() {
        return Err(AppError::BadRequest(
            "ask requires a non-empty question".into(),
        ));
    }

    let answer = noted_index::global_search::global_search(
        &st.pool,
        q.workspace_id,
        question,
        st.answerer.as_ref(),
        st.summariser.clone(),
    )
    .await
    .map_err(|e| AppError::AskFailed(format!("global search failed: {e}")))?;

    Ok(Json(answer))
}

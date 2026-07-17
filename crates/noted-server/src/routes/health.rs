use axum::extract::State;
use axum::Json;

use crate::error::AppError;
use crate::state::AppState;

pub async fn health(State(st): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    let version: String =
        sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
            .fetch_optional(&st.pool)
            .await?
            .ok_or(AppError::NotFound)?;

    Ok(Json(serde_json::json!({ "status": "ok", "pgvector": version })))
}

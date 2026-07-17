use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("not found")]
    NotFound,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        // Never leak SQL detail to clients; log it instead.
        if let AppError::Db(ref e) = self {
            tracing::error!(error = %e, "database error");
        }
        (status, Json(serde_json::json!({ "error": status.canonical_reason() })))
            .into_response()
    }
}

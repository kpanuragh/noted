use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("not found")]
    NotFound,
    #[error("pgvector {found} is below the required 0.8")]
    PgvectorTooOld { found: String },
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::PgvectorTooOld { .. } => StatusCode::SERVICE_UNAVAILABLE,
        };
        // Never leak SQL detail to clients; log it instead.
        if let AppError::Db(ref e) = self {
            tracing::error!(error = %e, "database error");
        }
        // Operator-facing diagnostic, not a leak: name the version and the floor.
        if let AppError::PgvectorTooOld { ref found } = self {
            return (
                status,
                Json(serde_json::json!({
                    "error": status.canonical_reason(),
                    "found": found,
                    "required": "0.8",
                })),
            )
                .into_response();
        }
        (status, Json(serde_json::json!({ "error": status.canonical_reason() })))
            .into_response()
    }
}

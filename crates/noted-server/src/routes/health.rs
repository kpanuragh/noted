use axum::extract::State;
use axum::Json;

use crate::error::AppError;
use crate::state::AppState;

/// Minimum required pgvector version (major, minor).
///
/// pgvector 0.8's iterative index scans are required for permission-filtered
/// retrieval later: without them, an HNSW scan with a WHERE clause overfilters
/// and silently returns fewer rows than requested. This is a security
/// foundation, so the health check must assert this at runtime.
pub const MIN_PGVECTOR: (u32, u32) = (0, 8);

/// Parse a pgvector `extversion` like "0.8.5" into (major, minor).
/// Returns None if the string is not in a recognisable numeric form.
pub fn parse_version(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

pub async fn health(State(st): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    let version: String =
        sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
            .fetch_optional(&st.pool)
            .await?
            .ok_or(AppError::NotFound)?;

    // Fail closed: an unparseable version does not meet the floor.
    match parse_version(&version) {
        Some(v) if v >= MIN_PGVECTOR => {
            Ok(Json(serde_json::json!({ "status": "ok", "pgvector": version })))
        }
        _ => Err(AppError::PgvectorTooOld { found: version }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_cases() {
        let meets = |s: &str| parse_version(s).is_some_and(|v| v >= MIN_PGVECTOR);

        assert!(meets("0.8.5"), "0.8.5 should meet the floor");
        assert!(!meets("0.7.4"), "0.7.4 should not meet the floor");
        assert!(meets("0.9.0"), "0.9.0 should meet the floor");
        assert!(meets("0.10.0"), "0.10.0 should meet the floor (numeric, not lexicographic)");
        assert!(meets("1.0.0"), "1.0.0 should meet the floor");
        assert!(!meets("garbage"), "garbage should fail closed and not meet the floor");
    }
}

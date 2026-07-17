use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct QuickHit {
    pub page_id: Uuid,
    pub title: String,
    pub rank: f32,
}

/// Navigational search: the user knows the page and is typing its name.
///
/// Deliberately LEXICAL only — no vector similarity. Embeddings would rank a
/// semantically-adjacent page above the exact title the user typed, which reads
/// as the product being broken. Exact match, then prefix, then trigram
/// similarity; `pages_title_trgm_idx` (built in M1a) serves the last.
pub async fn quick_find(
    pool: &PgPool,
    workspace_id: Uuid,
    q: &str,
    limit: i64,
) -> Result<Vec<QuickHit>, sqlx::Error> {
    let q = q.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }

    sqlx::query_as::<_, QuickHit>(
        "SELECT id AS page_id, title,
                CASE
                  WHEN lower(title) = lower($2)            THEN 1.0
                  WHEN lower(title) LIKE lower($2) || '%'  THEN 0.9
                  ELSE similarity(title, $2) * 0.8
                END::real AS rank
         FROM pages
         WHERE workspace_id = $1
           AND archived_at IS NULL
           AND (lower(title) LIKE '%' || lower($2) || '%' OR similarity(title, $2) > 0.15)
         ORDER BY rank DESC, title
         LIMIT $3",
    )
    .bind(workspace_id)
    .bind(q)
    .bind(limit)
    .fetch_all(pool)
    .await
}

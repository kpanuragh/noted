use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct QuickHit {
    pub page_id: Uuid,
    pub title: String,
    pub rank: f32,
}

/// Escape LIKE/ILIKE metacharacters so a user typing "50% done" or "Q4_2026"
/// searches for those literal characters rather than matching everything.
/// Backslash first, or it would double-escape the escapes we just added.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Navigational search: the user knows the page and is typing its name.
///
/// Deliberately LEXICAL only — no vector similarity. Embeddings would rank a
/// semantically-adjacent page above the exact title the user typed, which reads
/// as the product being broken. Exact match, then prefix, then trigram
/// similarity; `pages_title_trgm_idx` (built in M1a) serves the last.
///
/// The WHERE clause uses trgm-indexable operators (`ILIKE` and `%`) against
/// `title` directly, rather than wrapping `title` in `lower()` or calling
/// `similarity()` as a bare function — either of which would force a
/// sequential scan instead of using `pages_title_trgm_idx`.
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
    let escaped = escape_like(q);

    sqlx::query_as::<_, QuickHit>(
        "SELECT id AS page_id, title,
                CASE
                  WHEN lower(title) = lower($2)   THEN 1.0
                  WHEN title ILIKE $3 || '%'      THEN 0.9
                  ELSE similarity(title, $2) * 0.8
                END::real AS rank
         FROM pages
         WHERE workspace_id = $1
           AND archived_at IS NULL
           AND (title ILIKE '%' || $3 || '%' OR title % $2)
         ORDER BY rank DESC, title
         LIMIT $4",
    )
    .bind(workspace_id)
    .bind(q)
    .bind(&escaped)
    .bind(limit)
    .fetch_all(pool)
    .await
}

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

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct RelatedHit {
    pub page_id: Uuid,
    pub title: String,
    pub snippet: String,
    pub distance: f32,
}

/// Pages semantically near this one.
///
/// Needs NO model at request time: it compares this page's OWN stored chunk
/// embeddings against every other chunk's. That is what makes the related-notes
/// panel free — no inference in the request path.
///
/// `hnsw.iterative_scan` is set because this query FILTERS (workspace, self-
/// exclusion, model). Without it an HNSW scan with a WHERE clause overfilters:
/// it silently returns fewer rows than LIMIT asks for. It defaults to `off`.
///
/// Deliberate M1c simplification: only the page's FIRST chunk embedding is used
/// (`src LIMIT 1`). Averaging a page's chunk vectors, or taking the best match
/// across all of them, is a retrieval-quality question that can't be evaluated
/// until the related-notes panel exists and someone looks at real results.
pub async fn related_pages(
    pool: &PgPool,
    page_id: Uuid,
    model_id: &str,
    limit: i64,
) -> Result<Vec<RelatedHit>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL hnsw.iterative_scan = relaxed_order")
        .execute(&mut *tx)
        .await?;

    let hits = sqlx::query_as::<_, RelatedHit>(
        "WITH src AS (
             SELECT e.embedding
             FROM page_chunks pc
             JOIN embeddings e
               ON e.content_hash = pc.content_hash AND e.model_id = $2
             WHERE pc.page_id = $1
         ),
         ws AS (SELECT workspace_id FROM pages WHERE id = $1)
         SELECT p.id AS page_id, p.title, c.text AS snippet,
                MIN(e.embedding <=> (SELECT embedding FROM src LIMIT 1))::real AS distance
         FROM embeddings e
         JOIN chunks c        ON c.content_hash = e.content_hash
         JOIN page_chunks pc  ON pc.content_hash = c.content_hash
         JOIN pages p         ON p.id = pc.page_id
         WHERE e.model_id = $2
           AND p.id <> $1
           AND p.archived_at IS NULL
           AND p.workspace_id = (SELECT workspace_id FROM ws)
           AND EXISTS (SELECT 1 FROM src)
         GROUP BY p.id, p.title, c.text
         ORDER BY distance
         LIMIT $3",
    )
    .bind(page_id)
    .bind(model_id)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(hits)
}

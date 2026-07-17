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
/// Deliberate M1c simplification: only the page's FIRST chunk embedding is used
/// (`src ... LIMIT 1`). Averaging a page's chunk vectors, or taking the best
/// match across all of them, is a retrieval-quality question that can't be
/// evaluated until the related-notes panel exists and someone looks at real
/// results. The `ORDER BY pc.chunk_index` is for DETERMINISM — without it
/// "the first chunk" is whichever row Postgres happened to return, so the same
/// page could yield different related lists on successive calls.
///
/// # Why the query is shaped like this
///
/// pgvector can only use `embeddings_hnsw_idx` for `ORDER BY <vector> <=>
/// <const> LIMIT n` where that ordering comes DIRECTLY off the base-relation
/// scan and feeds straight into a Limit. Anything between the scan and the
/// sort — an aggregate, a join, a semi-join — destroys index-provided ordering
/// and silently turns this into a filtered full scan over every embedding.
/// So the ANN lives alone in `near`, and pages are resolved AFTERWARDS.
///
/// The workspace filter MUST stay INSIDE `near`. `embeddings` has no
/// `workspace_id` and cannot get one: chunks are content-addressed, so two
/// workspaces holding identical text legitimately SHARE a chunk and its
/// embedding. Overfetching globally and filtering by workspace afterwards
/// would let a large multi-tenant instance spend the whole top-N on other
/// tenants' pages and return nothing for this one.
///
/// That filtered ANN is exactly what `hnsw.iterative_scan` exists for: without
/// it an HNSW scan with a WHERE clause overfilters, silently returning fewer
/// rows than LIMIT asks for. It defaults to `off`, hence the `SET LOCAL`.
///
/// `OFFSET 0` in the workspace EXISTS is an OPTIMIZATION FENCE, not dead code.
/// Postgres otherwise pulls an EXISTS sublink up into a semi-join, which puts a
/// join between the scan and the sort and costs us the index — verified by
/// EXPLAIN: without it the plan is a `HashAggregate` + `Nested Loop` and never
/// mentions `embeddings_hnsw_idx`. `simplify_EXISTS_query` declines to pull up
/// a subquery carrying LIMIT/OFFSET, so the fence keeps it a SubPlan filter on
/// the index scan. `related_pages_uses_the_hnsw_index` locks this in.
///
/// `near` overfetches (`LIMIT $3 * 5`) because one chunk can belong to several
/// pages and one page to several chunks, so the ANN must yield more candidate
/// chunks than the final page count. `best` then collapses each page to its
/// single closest chunk (`DISTINCT ON (p.id)`) — `RelatedHit` promises related
/// PAGES, and without this a 5-chunk page produces 5 rows that each compete for
/// the LIMIT, letting one chunky page crowd out every other page.
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
             ORDER BY pc.chunk_index
             LIMIT 1
         ),
         ws AS (SELECT workspace_id FROM pages WHERE id = $1),
         near AS (
             SELECT e.content_hash,
                    (e.embedding <=> (SELECT embedding FROM src)) AS distance
             FROM embeddings e
             WHERE e.model_id = $2
               AND EXISTS (SELECT 1 FROM src)
               AND EXISTS (
                   SELECT 1
                   FROM page_chunks pc
                   JOIN pages p ON p.id = pc.page_id
                   WHERE pc.content_hash = e.content_hash
                     AND p.workspace_id = (SELECT workspace_id FROM ws)
                     AND p.id <> $1
                     AND p.archived_at IS NULL
                   OFFSET 0
               )
             ORDER BY e.embedding <=> (SELECT embedding FROM src)
             LIMIT $3::bigint * 5
         ),
         best AS (
             SELECT DISTINCT ON (p.id)
                    p.id AS page_id, p.title, c.text AS snippet, n.distance
             FROM near n
             JOIN chunks c       ON c.content_hash = n.content_hash
             JOIN page_chunks pc ON pc.content_hash = n.content_hash
             JOIN pages p        ON p.id = pc.page_id
             WHERE p.id <> $1
               AND p.archived_at IS NULL
               AND p.workspace_id = (SELECT workspace_id FROM ws)
             ORDER BY p.id, n.distance
         )
         SELECT page_id, title, snippet, distance::real
         FROM best
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

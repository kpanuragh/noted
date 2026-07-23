use sqlx::PgPool;
use crate::readable_pages_cte;
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
/// `user_id` filters to pages the caller may READ (M4-3).
///
/// Threaded through rather than applied afterwards: filtering the result set in
/// Rust would return fewer than `limit` rows whenever anything was hidden, and
/// a user with a denied subtree would silently get short pages of results with
/// no way to tell why. Applying it inside the query means `limit` counts only
/// rows the caller may actually see.
pub async fn quick_find(
    pool: &PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
    q: &str,
    limit: i64,
) -> Result<Vec<QuickHit>, sqlx::Error> {
    let q = q.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let escaped = escape_like(q);

    sqlx::query_as::<_, QuickHit>(concat!(
        "WITH ",
        readable_pages_cte!("$1", "$2"),
        " SELECT p.id AS page_id, p.title,
                 CASE
                   WHEN lower(p.title) = lower($3)   THEN 1.0
                   WHEN p.title ILIKE $4 || '%'      THEN 0.9
                   ELSE similarity(p.title, $3) * 0.8
                 END::real AS rank
          FROM pages p
          JOIN readable_pages r ON r.page_id = p.id
          WHERE p.workspace_id = $1
            AND p.archived_at IS NULL
            AND (p.title ILIKE '%' || $4 || '%' OR p.title % $3)
          ORDER BY rank DESC, p.title
          LIMIT $5"
    ))
    .bind(workspace_id)
    .bind(user_id)
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
///
/// # `embeddings_hnsw_idx` is approximate, not exact
///
/// HNSW is an ANN (approximate nearest-neighbour) index: it trades recall for
/// speed, governed by `hnsw.ef_search` (default 40). At a few dozen vectors per
/// `model_id` the approximation is exact in practice — the candidate list is
/// small enough that the graph search finds the true top-k. As the vector count
/// for a given `model_id` grows into the tens of thousands, recall degrades: a
/// genuinely-nearest neighbour can fall outside the returned top-k. This is
/// inherent to ANN, not a bug in this query. It means retrieval quality should
/// be measured (and `ef_search` tuned upward if needed) before concluding a
/// model's embeddings are bad.
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

use pgvector::Vector;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct SearchHit {
    pub page_id: Uuid,
    pub title: String,
    pub snippet: String,
    pub score: f32,
}

/// Reciprocal Rank Fusion constant. 60 is the value from the original paper and
/// is not worth tuning — RRF's appeal is that it needs no calibration.
const RRF_K: f32 = 60.0;

/// Hybrid search: a lexical arm and a vector arm, fused on RANK.
///
/// Neither arm alone is acceptable. Vector search is bad at precision (error
/// codes, surnames, flags, exact phrases — embeddings blur exactly what the user
/// is anchoring on). Lexical search is bad at recall (it cannot match "database
/// performance" to "the planner keeps choosing a seq scan").
///
/// Fusion is on rank, not score: `ts_rank_cd` and cosine distance are
/// incompatible scales, and normalising them against each other is the fragile
/// step that makes naive hybrid search unreliable. RRF ignores the scores:
/// `score = Σ 1/(k + rank_i)`, summed over whichever arm(s) found a page.
///
/// `q_vec` is supplied by the caller — this crate does no inference, which keeps
/// the embedding model out of `noted-db`.
///
/// # Why the vector arm is shaped like `related_pages`, not like a plain join
///
/// The obvious way to write the vector arm is to join `embeddings` straight to
/// `chunks`/`page_chunks`/`pages`, filter on workspace, and `ORDER BY distance
/// LIMIT n`. That is wrong: pgvector can only push ANN ordering into
/// `embeddings_hnsw_idx` for `ORDER BY <vector> <=> <const> LIMIT n` taken
/// DIRECTLY off the `embeddings` base-relation scan. Any join between that scan
/// and the sort — needed to resolve a workspace, since `embeddings` carries no
/// `workspace_id` — destroys the index-provided ordering and turns it into a
/// filtered full scan over every embedding in the instance. That is the exact
/// mistake `related_pages` (Task 2) hit and fixed; see its doc comment.
///
/// So the ANN lives ALONE in `near`: a plain `WHERE model_id = $4 AND EXISTS
/// (...)` scan ordered straight into `LIMIT`, with the workspace check pushed
/// into an `EXISTS` subquery. The `OFFSET 0` inside that `EXISTS` is an
/// OPTIMIZATION FENCE, not dead code — `simplify_EXISTS_query` declines to pull
/// up a subquery carrying LIMIT/OFFSET, which is what keeps Postgres from
/// hoisting the `EXISTS` into a semi-join (a join that would sit between the
/// scan and the sort and cost us the index, exactly as in `related_pages`).
///
/// The filter must live INSIDE `near`, not after: chunks are content-addressed
/// and shared across tenants, so overfetching globally and filtering by
/// workspace afterwards could spend the whole candidate budget on other
/// tenants' rows and return nothing for this workspace.
///
/// `vec_pages` then resolves `near`'s content-hash candidates to pages and
/// collapses each page to its single closest chunk (`DISTINCT ON (p.id)`) —
/// one chunk can belong to several pages and one page to several chunks, so
/// without this a multi-chunk page would produce several rows that each
/// compete for a rank in the fusion.
///
/// Like `related_pages`, the vector arm's `near` CTE rides `embeddings_hnsw_idx`,
/// which is an APPROXIMATE index (see that function's doc comment). At scale,
/// recall for a given `model_id` is bounded by `hnsw.ef_search`, not just by the
/// candidate LIMIT here — a real near neighbour can be missing from `near`
/// entirely. The lexical arm's `UNION ALL` in `fused` is unaffected either way.
/// `user_id` filters to pages the caller may READ (M4-3).
///
/// Applied INSIDE both arms rather than to the fused result: the lexical arm
/// caps at 50 rows and the vector arm at 100 BEFORE fusion, so filtering
/// afterwards would let denied pages consume that budget and silently shorten
/// what a permitted user sees.
///
/// In the vector arm it rides in the SAME `EXISTS` that already carries the
/// workspace and archived checks — the shape proven to survive HNSW pushdown
/// with `hnsw.iterative_scan = relaxed_order`. Adding a separate join around the
/// ANN scan is exactly how this repo produced three "looks-indexed-but-isn't"
/// bugs, so the filter goes where the existing one is known to work.
/// The furthest a chunk may be from the query and still count as a vector hit.
///
/// WITHOUT THIS the vector arm is a pure k-nearest-neighbours query: it returns
/// the closest N rows no matter how far away they are, so it ALWAYS returns
/// something. In a workspace with two pages, every query — including one whose
/// words appear nowhere — came back with both pages, because both are trivially
/// inside the top 100. "Nearest" is not "relevant", and a search that cannot
/// return nothing cannot be trusted when it returns something.
///
/// Measured with `noted-index`'s `distance_calibration` test against a real
/// workspace (bge-base-en-v1.5, pgvector cosine distance, so the range is
/// [0, 2]):
///
/// ```text
/// query                            closest chunk
/// Kerala beaches                        0.238   <- relevant
/// dinosaurs                             0.271   <- relevant
/// asdfghjkl qwerty                      0.518   <- nonsense
/// Data                                  0.548   <- the reported bug
/// banking regulation compliance         0.578   <- plausible but unrelated
/// ```
///
/// 0.45 sits in the empty gap: 0.18 clear of the furthest relevant hit and 0.07
/// clear of the nearest irrelevant one.
///
/// Two caveats worth keeping honest. This is calibrated on ONE small workspace,
/// so the gap is measured rather than proven general. And the same numbers
/// taken across an entire shared database instead of one workspace do NOT
/// separate at all — nonsense scored 0.299 there against 0.271 for a genuine
/// query — because a corpus-wide population is not the one a user searches.
/// Re-run the calibration before changing this number.
pub const MAX_COSINE_DISTANCE: f64 = 0.45;

pub async fn hybrid(
    pool: &PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
    q: &str,
    q_vec: &[f32],
    model_id: &str,
    limit: i64,
) -> Result<Vec<SearchHit>, sqlx::Error> {
    let q = q.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }

    let mut tx = pool.begin().await?;
    // The vector arm filters (via the EXISTS in `near`), so without iterative
    // scans an HNSW scan under a WHERE clause overfilters and silently returns
    // short.
    sqlx::query("SET LOCAL hnsw.iterative_scan = relaxed_order")
        .execute(&mut *tx)
        .await?;

    let hits = sqlx::query_as::<_, SearchHit>(concat!(
        "WITH ", readable_pages_cte!("$1", "$7"), ",
         lex AS (
             SELECT b.page_id, b.text AS snippet,
                    ROW_NUMBER() OVER (
                        ORDER BY ts_rank_cd(to_tsvector('english', b.text),
                                            plainto_tsquery('english', $2)) DESC,
                                 b.page_id
                    ) AS rank
             FROM blocks b
             JOIN pages p ON p.id = b.page_id
             JOIN readable_pages r ON r.page_id = p.id
             WHERE p.workspace_id = $1
               AND p.archived_at IS NULL
               AND to_tsvector('english', b.text) @@ plainto_tsquery('english', $2)
             LIMIT 50
         ),
         near AS (
             SELECT e.content_hash,
                    (e.embedding <=> $3) AS distance
             FROM embeddings e
             WHERE e.model_id = $4
               -- The cutoff. See MAX_COSINE_DISTANCE: without it this arm can
               -- never return empty, so every query matches every document.
               AND (e.embedding <=> $3) < $8
               AND EXISTS (
                   SELECT 1
                   FROM page_chunks pc
                   JOIN pages p ON p.id = pc.page_id
                   JOIN readable_pages r ON r.page_id = p.id
                   WHERE pc.content_hash = e.content_hash
                     AND p.workspace_id = $1
                     AND p.archived_at IS NULL
                   OFFSET 0
               )
             ORDER BY e.embedding <=> $3
             LIMIT 100
         ),
         vec_pages AS (
             SELECT DISTINCT ON (p.id)
                    p.id AS page_id, c.text AS snippet, n.distance
             FROM near n
             JOIN chunks c       ON c.content_hash = n.content_hash
             JOIN page_chunks pc ON pc.content_hash = n.content_hash
             JOIN pages p        ON p.id = pc.page_id
             JOIN readable_pages r ON r.page_id = p.id
             WHERE p.workspace_id = $1
               AND p.archived_at IS NULL
             ORDER BY p.id, n.distance
         ),
         vec AS (
             SELECT page_id, snippet,
                    ROW_NUMBER() OVER (ORDER BY distance, page_id) AS rank
             FROM vec_pages
         ),
         fused AS (
             SELECT page_id, snippet, rank, TRUE  AS is_lex FROM lex
             UNION ALL
             SELECT page_id, snippet, rank, FALSE AS is_lex FROM vec
         )
         SELECT f.page_id, p.title,
                (array_agg(f.snippet ORDER BY f.rank))[1] AS snippet,
                SUM((1.0::real) / ($5 + f.rank::real))::real AS score
         FROM fused f
         JOIN pages p ON p.id = f.page_id
         GROUP BY f.page_id, p.title
         -- Ties are COMMON, not exotic: RRF scores depend only on rank, so a
         -- page at lexical rank 1 scores exactly what a different page at
         -- vector rank 1 scores. Ordering by score alone left those ties to
         -- Postgres, which is free to return them in any order — a search that
         -- reorders its own results between identical queries.
         --
         -- Broken toward the lexical arm, deliberately. If someone types a rare
         -- exact term, the page containing that literal string is what they
         -- asked for; a semantic neighbour that merely embeds nearby is a guess.
         -- page_id last so the order is TOTAL and therefore reproducible.
         ORDER BY score DESC, bool_or(f.is_lex) DESC, f.page_id
         LIMIT $6"
    ))
    .bind(workspace_id)
    .bind(q)
    .bind(Vector::from(q_vec.to_vec()))
    .bind(model_id)
    .bind(RRF_K)
    .bind(limit)
    .bind(user_id)
    .bind(MAX_COSINE_DISTANCE)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(hits)
}

//! Local (entity-anchored) graph retrieval — M2c Task 1.
//!
//! PURE RETRIEVAL. This module answers "which chunks does the graph say are
//! relevant, and why", and nothing else. Synthesis (`AnswerProvider`) lives in
//! `noted-index`; like `graph` and `community`, this module deals only in
//! primitives so `noted-db` never has to depend back on `noted-index`.
//!
//! # The pipeline, and why it starts at CHUNKS
//!
//! ```text
//! seed chunks (caller-supplied; M1c `search::hybrid` produces them)
//!   -> seed entities  = entities anchored by a clusterable edge whose
//!                       source_chunk_hash is a seed chunk
//!   -> traverse 1..MAX_HOPS over CLUSTERABLE edges (recursive CTE)
//!   -> the chunks that evidence each traversed edge
//!   -> rank, collapse to one row per (page, chunk)
//! ```
//!
//! Product design §6.4 says to seed entities by vector similarity on entity
//! DESCRIPTIONS. Spec §2.1 records why M2c does not: nothing embeds
//! `entities.description` (the `embeddings` table is keyed by chunk content
//! hash), and that column is NULL for essentially every row the stub extractor
//! writes. Seeding there would produce a search that silently returns nothing.
//! Chunks are already embedded and already searchable, so they are the seam.
//!
//! It also gives the right degradation for free: with an empty graph the
//! traversal contributes nothing and `local_search_chunks` returns exactly the
//! seed chunks — local search becomes hybrid search rather than becoming
//! useless. `an_empty_graph_degrades_to_the_seed_chunks` pins that.
use sqlx::PgPool;
use uuid::Uuid;

/// How far the traversal may walk from a seed entity. **A hard constant, not a
/// parameter.**
///
/// An unbounded (or caller-chosen) traversal over a dense graph is a trivial
/// self-DoS: the neighbourhood of a hub entity at depth 4 can be the entire
/// workspace, and the recursive CTE would materialise every path to it. This
/// codebase already caps `limit` in the repository rather than in the HTTP
/// handler (`pages::MAX_RECENT_LIMIT`) for the same reason — a second entry
/// point that forgot to clamp would otherwise reopen the hole — so the cap
/// belongs here, where every caller inherits it.
///
/// 2 is the depth at which the result is still explainable to a user ("this
/// page is about someone your note mentions"). At 3 the provenance chain stops
/// being an explanation and starts being a coincidence.
pub const MAX_HOPS: i32 = 2;

/// Results are clamped to `1..=MAX_LOCAL_LIMIT`, same discipline and same
/// reason as `pages::MAX_RECENT_LIMIT`.
pub const MAX_LOCAL_LIMIT: i64 = 100;

/// Reciprocal Rank Fusion constant, matching `search::RRF_K`. Seed ranks
/// arriving here come out of M1c's RRF fusion, so the curve that converts a
/// rank back into a magnitude has to be the same one.
const RRF_K: f64 = 60.0;

/// Per-hop multiplicative discount. See `local_search_chunks`' ranking section:
/// 0.5 is chosen to be LARGER than the entire spread of the seed-rank curve, so
/// hop distance is the primary sort key by construction rather than by luck.
const HOP_DECAY: f64 = 0.5;

/// A chunk that seeded the search, with its 1-based rank in the seed ranking.
///
/// `rank`, not score: M1c's `hybrid` fuses on rank precisely because
/// `ts_rank_cd` and cosine distance are incompatible scales, and re-introducing
/// a raw score here would undo that. Rank 1 is the best seed.
#[derive(Debug, Clone)]
pub struct SeedChunk {
    pub content_hash: String,
    pub rank: i32,
}

/// One retrieved chunk, with the provenance a citation needs.
///
/// `hops` IS the "why this is here" the product surface shows: 0 means the
/// chunk was a seed (hybrid search found it directly), 1 or 2 means the graph
/// reached it — the chunk evidences an edge `hops` steps from a seed entity.
/// That field is not debug output; spec §3 makes "show your work" the point.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct GraphHit {
    pub page_id: Uuid,
    pub content_hash: String,
    pub title: String,
    pub snippet: String,
    pub hops: i32,
    pub score: f32,
}

/// The entities the seed chunks anchor — the "why these results" surface.
///
/// An entity is anchored by a seed chunk when a CLUSTERABLE edge extracted from
/// that chunk names it at either end. Both ends count: extraction direction is
/// an artefact of how the model phrased the relation, not a statement about
/// which entity the chunk is "about".
///
/// Ordered by `name` for the same reason `community::clusterable_graph` is:
/// `entities.id` is `gen_random_uuid()`, so id order is not reproducible.
pub async fn seed_entities(
    pool: &PgPool,
    workspace_id: Uuid,
    seeds: &[SeedChunk],
) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
    if seeds.is_empty() {
        return Ok(Vec::new());
    }
    let hashes: Vec<String> = seeds.iter().map(|s| s.content_hash.clone()).collect();

    sqlx::query_as(concat!(
        "WITH ",
        crate::community::clusterable_edges_cte!(),
        " SELECT DISTINCT en.id, en.name
          FROM clusterable_edges ce
          JOIN entities en
            ON (en.id = ce.source_entity OR en.id = ce.target_entity)
           AND en.workspace_id = $1
          WHERE ce.source_chunk_hash = ANY($2)
          ORDER BY en.name ASC"
    ))
    .bind(workspace_id)
    .bind(&hashes)
    .fetch_all(pool)
    .await
}

/// Local search: seed chunks in, ranked chunks (seeds AND graph-reached) out.
///
/// # Every traversed edge is CLUSTERABLE
///
/// The traversal splices `clusterable_edges_cte!` — the macro M2b introduced so
/// that "live" has exactly one definition. Local search is its sixth consumer.
/// Paraphrasing it here is how this project produced three tenancy bugs: the
/// definition carries BOTH the workspace scope (`e.workspace_id = $1`) and the
/// archived-page exclusion, and the `p.workspace_id = e.workspace_id`
/// correlation inside it is what stops one tenant's live page keeping another
/// tenant's archived content reachable through a shared content-addressed chunk.
///
/// # Ranking
///
/// ```text
/// score = 1/(RRF_K + seed_rank) * HOP_DECAY^hops * bottleneck_weight
/// ```
///
/// What is GUARANTEED are three monotonicities, each holding the other two
/// factors fixed. They are what
/// `ranking_is_monotone_in_hops_seed_rank_and_edge_weight` asserts, as relative
/// ORDERING rather than as scores:
///
///   * **Closer is stronger.** `HOP_DECAY = 0.5 < 1`, so at equal seed rank and
///     equal path weight, a seed outranks a 1-hop hit outranks a 2-hop hit.
///   * **A better seed is stronger.** `1/(RRF_K + rank)` is strictly decreasing
///     in `rank`, so at equal hops and weight, a hit reached from the best seed
///     chunk beats one reached from a worse seed. A chunk reachable by several
///     paths keeps its BEST path's score (and that path's `hops`).
///   * **A stronger path is stronger.** A path's weight is its BOTTLENECK — the
///     minimum weight along it — because a chain is only as good as its weakest
///     link, and because a bottleneck is monotone: adding a hop can never make a
///     path stronger.
///
/// What is deliberately NOT guaranteed is that the three form strict tiers. A
/// 2-hop hit off the top seed CAN outrank a 1-hop hit off a very poor seed
/// (rank 100), and a full-weight 2-hop hit CAN outrank a weight-0.1 1-hop hit.
/// Both are asserted, not merely tolerated. Making hop distance lexicographically
/// dominant instead would require bounding the caller-supplied seed ranks, which
/// this module does not control — and the trade is not obviously right: a chunk
/// hybrid search ranked 100th is weak direct evidence, and one confident graph
/// hop off the best seed is a reasonable thing to prefer.
///
/// The bottleneck recursion starts at `1.0`, which also CAPS it. LIMITATION,
/// recorded rather than hidden: weight is therefore a confidence *discount* in
/// `(0, 1]`, and an extractor emitting weights above 1 to mean "very confident"
/// would find them flattened. Nothing emits such weights today
/// (`replace_chunk_edges` stores whatever the extractor gives; `StubExtractor`
/// gives 1.0). The cap exists so that a single heavy edge cannot overturn hop
/// decay outright, which would make "closer is stronger" untrue rather than
/// merely non-lexicographic.
///
/// # Deduplication
///
/// A chunk reachable by many paths appears ONCE, carrying its best-scoring
/// path's `hops`. Then, because one chunk can sit on several pages (chunks are
/// content-addressed and shared) and one page can hold the same chunk twice,
/// the final projection is `DISTINCT ON (page_id, content_hash)`. The seeds are
/// unioned in at `hops = 0` and win any tie against a path that walks back
/// round to them.
///
/// # Liveness and tenancy of the OUTPUT
///
/// The traversal's tenancy comes from the spliced macro; the projection re-states
/// `p.workspace_id = $1 AND p.archived_at IS NULL` because the seed chunks are
/// caller-supplied content hashes and content hashes are GLOBAL. Without it, a
/// caller handing over a hash that another tenant also holds would have that
/// tenant's page resolved and returned. That predicate is load-bearing on the
/// seed path specifically, and `seed_chunks_never_resolve_to_another_workspaces_page`
/// deletes it to prove so.
/// `user_id` filters every returned passage to pages the caller may READ.
///
/// This is where the graph creates a leak that plain search does not: the
/// traversal reaches chunks through `page_chunks` AFTER walking edges, so a page
/// that search itself would never return can still be pulled in because
/// something readable is connected to it. Filtering here closes the hop.
pub async fn local_search_chunks(
    pool: &PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
    seeds: &[SeedChunk],
    limit: i64,
) -> Result<Vec<GraphHit>, sqlx::Error> {
    if seeds.is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(1, MAX_LOCAL_LIMIT);
    let hashes: Vec<String> = seeds.iter().map(|s| s.content_hash.clone()).collect();
    let ranks: Vec<i32> = seeds.iter().map(|s| s.rank).collect();

    sqlx::query_as(concat!(
        "WITH RECURSIVE ",
        crate::readable_pages_cte!("$1", "$8"),
        ",
        ",
        crate::community::clusterable_edges_cte!(),
        ",
        -- The caller CAN repeat a hash: `hybrid` returns pages, and one chunk
        -- can sit on two of them, so the page->chunk mapping legitimately emits
        -- the same hash at two ranks. `MIN(r)` keeps the better one.
        -- NOT LOAD-BEARING, and measured so: deleting the GROUP BY kills no
        -- test, because `best`'s DISTINCT ON collapses the duplicate anyway and
        -- `anchors` already takes MIN over seed ranks. It stays because it makes
        -- the recursion start from a set rather than a bag, which is what the
        -- rest of this query assumes, and it costs one hash aggregate.
        seeds AS (
            SELECT h AS content_hash, MIN(r) AS seed_rank
            FROM UNNEST($2::text[], $3::int[]) AS x(h, r)
            GROUP BY h
        ),
        -- Entities a seed chunk anchors. Both endpoints: extraction direction
        -- says nothing about which entity the chunk is about.
        anchors AS (
            SELECT v.entity_id, MIN(s.seed_rank) AS seed_rank
            FROM clusterable_edges ce
            JOIN seeds s ON s.content_hash = ce.source_chunk_hash
            CROSS JOIN LATERAL (VALUES (ce.source_entity), (ce.target_entity))
                 AS v(entity_id)
            GROUP BY v.entity_id
        ),
        -- The traversal. `hops < $4` is the ONLY termination condition, so the
        -- cap is what makes this terminate at all on a cyclic graph.
        walk AS (
            SELECT a.entity_id,
                   a.seed_rank,
                   0 AS hops,
                   1.0::double precision AS bottleneck,
                   NULL::text AS via_chunk
            FROM anchors a
          UNION ALL
            SELECT CASE WHEN ce.source_entity = w.entity_id
                        THEN ce.target_entity ELSE ce.source_entity END,
                   w.seed_rank,
                   w.hops + 1,
                   LEAST(w.bottleneck, ce.weight::double precision),
                   ce.source_chunk_hash
            FROM walk w
            JOIN clusterable_edges ce
              ON ce.source_entity = w.entity_id OR ce.target_entity = w.entity_id
            WHERE w.hops < $4
        ),
        -- Seeds are evidence in their own right, at hop 0. This branch is what
        -- makes an empty graph degrade to plain hybrid rather than to nothing.
        evidence AS (
            SELECT s.content_hash, s.seed_rank, 0 AS hops,
                   1.0::double precision AS bottleneck
            FROM seeds s
          UNION ALL
            -- `hops > 0` is NOT LOAD-BEARING, and measured so: a hop-0 walk row
            -- carries a NULL `via_chunk` (it traversed no edge), so the join to
            -- `chunks` in `resolved` would drop it regardless. Deleting the
            -- predicate kills no test. It stays because -- a seed's evidence is
            -- the seed branch above, never the walk -- is the invariant this
            -- query is built on, and relying on a NULL to enforce it silently is
            -- how the next edit to the walk breaks it.
            SELECT w.via_chunk, w.seed_rank, w.hops, w.bottleneck
            FROM walk w
            WHERE w.hops > 0
        ),
        best AS (
            SELECT DISTINCT ON (content_hash) content_hash, hops, score
            FROM (
                SELECT content_hash, hops,
                       (1.0 / ($5 + seed_rank::double precision))
                         * power($6::double precision, hops::double precision)
                         * bottleneck AS score
                FROM evidence
            ) e
            ORDER BY content_hash, score DESC, hops ASC
        ),
        resolved AS (
            SELECT DISTINCT ON (p.id, b.content_hash)
                   p.id AS page_id, b.content_hash, p.title,
                   c.text AS snippet, b.hops, b.score
            FROM best b
            JOIN chunks c       ON c.content_hash = b.content_hash
            JOIN page_chunks pc ON pc.content_hash = b.content_hash
            JOIN pages p        ON p.id = pc.page_id
            JOIN readable_pages r ON r.page_id = p.id
            WHERE p.workspace_id = $1
              AND p.archived_at IS NULL
            ORDER BY p.id, b.content_hash
        )
        SELECT page_id, content_hash, title, snippet, hops, score::real
        FROM resolved
        ORDER BY score DESC, hops ASC, page_id
        LIMIT $7"
    ))
    .bind(workspace_id)
    .bind(&hashes)
    .bind(&ranks)
    .bind(MAX_HOPS)
    .bind(RRF_K)
    .bind(HOP_DECAY)
    .bind(limit)
    .bind(user_id)
    .fetch_all(pool)
    .await
}

//! M2c Task 1 — local (entity-anchored) graph retrieval.
//!
//! The crown jewel is `local_search_finds_a_page_that_hybrid_search_ranks_out`.
//! Everything else exists to stop it being true by accident.

use noted_db::graph_search::{self, GraphHit, SeedChunk};
use uuid::Uuid;

/// A fresh, never-before-used `model_id` per test — the same helper (and
/// rationale) as `hybrid.rs` and `related.rs`. `embeddings_hnsw_idx` is an
/// APPROXIMATE index whose recall degrades with the vector count under one
/// `model_id`; a unique id per test keeps each test's vector space small enough
/// that ANN search is exact for it.
fn unique_model() -> String {
    format!("gs-model-{}", Uuid::new_v4())
}

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

/// Every fixture gets its OWN workspace. Tests share a dev database, so
/// workspace scoping is what keeps one test's entities out of another's
/// traversal — and `entities`' `UNIQUE (workspace_id, name)` out of the way.
async fn workspace(pool: &noted_db::PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('gs-test') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A live page carrying one block (so FTS can see it) and one chunk.
/// Returns `(page_id, content_hash)`.
async fn page(pool: &noted_db::PgPool, ws: Uuid, title: &str, text: &str) -> (Uuid, String) {
    let id: Uuid =
        sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
            .bind(ws)
            .bind(title)
            .fetch_one(pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
         VALUES ($1, 0, 'paragraph', $2, md5($2))",
    )
    .bind(id)
    .bind(text)
    .execute(pool)
    .await
    .unwrap();
    let hash = format!("gs-{}", Uuid::new_v4());
    noted_db::chunks::upsert(pool, &[(hash.clone(), text.to_string(), 10)])
        .await
        .unwrap();
    noted_db::chunks::set_page_chunks(pool, id, &[hash.clone()])
        .await
        .unwrap();
    (id, hash)
}

/// As `page`, plus an embedding on one axis of the 768-dim space, so the page is
/// reachable by `hybrid`'s vector arm at a controlled distance.
async fn embedded_page(
    pool: &noted_db::PgPool,
    ws: Uuid,
    title: &str,
    text: &str,
    axis: usize,
    model: &str,
) -> (Uuid, String) {
    let (id, hash) = page(pool, ws, title, text).await;
    let mut v = vec![0.0f32; 768];
    v[axis] = 1.0;
    noted_db::chunks::store_embedding(pool, &hash, model, &v)
        .await
        .unwrap();
    (id, hash)
}

fn vec_at(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; 768];
    v[axis] = 1.0;
    v
}

async fn archive(pool: &noted_db::PgPool, page_id: Uuid) {
    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(page_id)
        .execute(pool)
        .await
        .unwrap();
}

/// A uniquely-named entity — unique because `entities` is `UNIQUE (workspace_id,
/// name)` and fixtures share a database.
async fn entity(pool: &noted_db::PgPool, ws: Uuid, label: &str) -> Uuid {
    noted_db::graph::resolve_entity(
        pool,
        ws,
        &format!("{label}-{}", Uuid::new_v4()),
        Some("CONCEPT"),
        None,
    )
    .await
    .unwrap()
}

/// One edge, extracted from `chunk`. Goes through `graph::replace_chunk_edges`
/// so the fixture is built by the same production writer the extraction worker
/// uses — a hand-rolled INSERT could set a `workspace_id` no writer ever sets.
async fn edge(
    pool: &noted_db::PgPool,
    ws: Uuid,
    chunk: &str,
    model: &str,
    a: Uuid,
    b: Uuid,
    weight: f32,
) {
    noted_db::graph::replace_chunk_edges(
        pool,
        ws,
        chunk,
        model,
        &[(a, b, "relates_to".to_string(), weight)],
    )
    .await
    .unwrap();
}

fn pos(hits: &[GraphHit], page_id: Uuid) -> Option<usize> {
    hits.iter().position(|h| h.page_id == page_id)
}

fn has(hits: &[GraphHit], page_id: Uuid) -> bool {
    pos(hits, page_id).is_some()
}

/// Seeds straight from a `hybrid` result list: rank is 1-based position, and the
/// chunk hashes are the page's own. This is exactly the mapping `noted-index`
/// will do in Task 2 — `hybrid` returns PAGES, `local_search_chunks` consumes
/// CHUNKS, and something has to bridge them.
async fn seeds_from_hybrid(
    pool: &noted_db::PgPool,
    hits: &[noted_db::search::SearchHit],
) -> Vec<SeedChunk> {
    let mut out = Vec::new();
    for (i, h) in hits.iter().enumerate() {
        let hashes: Vec<String> = sqlx::query_scalar(
            "SELECT content_hash FROM page_chunks WHERE page_id = $1 ORDER BY chunk_index",
        )
        .bind(h.page_id)
        .fetch_all(pool)
        .await
        .unwrap();
        for hash in hashes {
            out.push(SeedChunk {
                content_hash: hash,
                rank: i as i32 + 1,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// THE CROWN JEWEL
// ---------------------------------------------------------------------------

/// **The whole milestone rests on this.** Local search must surface a page that
/// hybrid search does not — one connected to the question's subject ONLY through
/// a graph edge, whose own text contains none of the question's terms.
///
/// # The fixture, and why the premise is honest
///
/// `ALPHA` is the question's subject: it contains the query terms and is
/// embedded. `BETA` is the payoff: its text ("sourdough starter…") shares not
/// one term with the query, and its embedding sits on a far axis. The only thing
/// tying BETA to the question is the graph — `ALPHA`'s chunk extracted an edge
/// naming a person, and `BETA`'s chunk extracted an edge naming that same
/// person's hobby.
///
/// Five DECOY pages are embedded EXACTLY on the query vector's axis (cosine
/// distance 0) with text that matches nothing. They exist because a test
/// workspace is small: hybrid's vector arm takes the top 100 candidates, so with
/// two pages BETA would be returned by hybrid trivially and the premise would be
/// a lie about workspace size rather than a statement about ranking.
///
/// So the premise is asserted TWICE, and the second assertion is the one that
/// cannot rot:
///
///   1. `hybrid(limit = 6)` does not contain BETA — the flat, checkable premise.
///   2. `hybrid(limit = 50)` — a limit larger than the entire workspace — ranks
///      BETA BELOW EVERY DECOY. That is the real claim, and it is independent of
///      the chosen `k`: hybrid has no signal that BETA is relevant, so it sorts
///      BETA beneath pages that are relevant to nothing at all. There is no
///      cutoff that admits BETA without also admitting pure noise.
///
/// If someone later gives BETA matching text or a nearer embedding, assertion
/// (2) fails and this test tells them the fixture stopped testing the thing.
///
/// MECHANISM PROTECTED: the recursive traversal itself. Deleted (by forcing the
/// recursive term to produce nothing) this test fails — BETA is unreachable and
/// local search returns only what hybrid already had.
#[tokio::test]
async fn local_search_finds_a_page_that_hybrid_search_ranks_out() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = unique_model();

    // The question. ALPHA carries its terms; nothing else does.
    let question = "ECONNREFUSED deployment incident";
    let q_vec = vec_at(700);

    let (alpha, alpha_chunk) = embedded_page(
        &pool,
        ws,
        "Postmortem",
        "the ECONNREFUSED deployment incident was diagnosed on friday",
        0,
        &model,
    )
    .await;

    // No shared term with the question, and a far-away embedding.
    let (beta, beta_chunk) = embedded_page(
        &pool,
        ws,
        "Weekend",
        "sourdough starter needs feeding twice daily in warm weather",
        500,
        &model,
    )
    .await;

    // Decoys sit ON the query vector, so they outrank BETA in the vector arm.
    let mut decoys = Vec::new();
    for i in 0..5 {
        let (id, _) = embedded_page(
            &pool,
            ws,
            &format!("Decoy{i}"),
            &format!("miscellaneous jottings number {i} with nothing in common"),
            700,
            &model,
        )
        .await;
        decoys.push(id);
    }

    // The graph: ALPHA's chunk names the incident and the person who fixed it;
    // BETA's chunk names that person and what they do at weekends. BETA is one
    // hop from the question's subject and zero words from it.
    let incident = entity(&pool, ws, "incident").await;
    let person = entity(&pool, ws, "person").await;
    let hobby = entity(&pool, ws, "hobby").await;
    edge(&pool, ws, &alpha_chunk, &model, incident, person, 1.0).await;
    edge(&pool, ws, &beta_chunk, &model, person, hobby, 1.0).await;

    // --- PREMISE (1): at the k a user would actually ask for, hybrid misses it.
    let hybrid_k = noted_db::search::hybrid(&pool, ws, question, &q_vec, &model, 6)
        .await
        .unwrap();
    assert!(
        hybrid_k.iter().any(|h| h.page_id == alpha),
        "fixture broken: hybrid must find the question's own subject"
    );
    assert!(
        !hybrid_k.iter().any(|h| h.page_id == beta),
        "PREMISE: plain hybrid search must NOT return the graph-only page — \
         if it does, this test proves nothing about the graph"
    );

    // --- PREMISE (2): and no larger k rescues it, because hybrid ranks it
    // below pages that are relevant to nothing.
    let hybrid_all = noted_db::search::hybrid(&pool, ws, question, &q_vec, &model, 50)
        .await
        .unwrap();
    let beta_pos = hybrid_all
        .iter()
        .position(|h| h.page_id == beta)
        .expect("fixture broken: BETA must be live, embedded and visible to hybrid at all");
    for d in &decoys {
        let d_pos = hybrid_all
            .iter()
            .position(|h| h.page_id == *d)
            .expect("fixture broken: decoys must be visible to hybrid");
        assert!(
            d_pos < beta_pos,
            "PREMISE: hybrid must rank the graph-only page BELOW irrelevant decoys — \
             otherwise some ordinary k would have found it and the graph adds nothing"
        );
    }

    // --- THE PAYOFF.
    let seeds = seeds_from_hybrid(&pool, &hybrid_k).await;
    let local = graph_search::local_search_chunks(&pool, ws, &seeds, 50)
        .await
        .unwrap();

    let hit = local
        .iter()
        .find(|h| h.page_id == beta)
        .expect("local search MUST surface the page reachable only through the graph");
    assert_eq!(
        hit.hops, 1,
        "and it must report WHY: one hop from a seed entity, not a direct match"
    );
    assert_eq!(hit.content_hash, beta_chunk);
}

// ---------------------------------------------------------------------------
// Degradation
// ---------------------------------------------------------------------------

/// With NO edges at all, local search must return the seed chunks — spec §2.1's
/// whole argument for seeding from chunks is that local search degrades to
/// hybrid search rather than to nothing.
///
/// MECHANISM PROTECTED: the `seeds` branch of the `evidence` UNION. Delete it
/// and this test returns zero rows.
#[tokio::test]
async fn an_empty_graph_degrades_to_the_seed_chunks() {
    let pool = pool().await;
    let ws = workspace(&pool).await;

    let (p1, c1) = page(&pool, ws, "One", "first note").await;
    let (p2, c2) = page(&pool, ws, "Two", "second note").await;

    let edge_count: i64 = sqlx::query_scalar("SELECT count(*) FROM edges WHERE workspace_id = $1")
        .bind(ws)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        edge_count, 0,
        "fixture premise: this workspace has NO graph"
    );

    let seeds = vec![
        SeedChunk {
            content_hash: c1.clone(),
            rank: 1,
        },
        SeedChunk {
            content_hash: c2.clone(),
            rank: 2,
        },
    ];
    let hits = graph_search::local_search_chunks(&pool, ws, &seeds, 50)
        .await
        .unwrap();

    assert_eq!(
        hits.len(),
        2,
        "an empty graph must return the seeds, not nothing"
    );
    assert_eq!(hits[0].page_id, p1, "seed rank 1 first");
    assert_eq!(hits[1].page_id, p2);
    assert!(hits.iter().all(|h| h.hops == 0), "seeds are hop 0");
}

// ---------------------------------------------------------------------------
// The depth cap
// ---------------------------------------------------------------------------

/// `MAX_HOPS` is a hard constant, and this proves it BITES — a chunk three hops
/// out is absent while the chunk two hops out is present.
///
/// # Why a chain of FIVE entities, not four
///
/// The plan says "a chain of 4 entities: a 2-hop search must not return the
/// 4th". That arithmetic is off by one against the anchoring rule, and the rule
/// is right: an edge has two endpoints, and a seed chunk that extracted the edge
/// `E1—E2` anchors BOTH E1 and E2 at hop 0 (extraction direction is an artefact
/// of phrasing, not a claim about which entity the chunk is about). So E4 is
/// only two hops from E2 and is legitimately in range. A five-entity chain puts
/// E5's evidence at hop 3 and makes the cap the reason it is missing.
///
/// Asserting BOTH sides matters: "hop 3 absent" alone would also pass if the
/// traversal were broken outright, which is why "hop 2 present" is asserted
/// first.
///
/// MECHANISM PROTECTED: `WHERE w.hops < $4` and the `MAX_HOPS` value bound into
/// it. Raise `MAX_HOPS` to 3 and this test fails.
#[tokio::test]
async fn the_traversal_stops_at_max_hops() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = unique_model();

    let (_, c12) = page(&pool, ws, "P12", "chain link one two").await;
    let (p23, c23) = page(&pool, ws, "P23", "chain link two three").await;
    let (p34, c34) = page(&pool, ws, "P34", "chain link three four").await;
    let (p45, c45) = page(&pool, ws, "P45", "chain link four five").await;

    let e: Vec<Uuid> = {
        let mut v = Vec::new();
        for i in 1..=5 {
            v.push(entity(&pool, ws, &format!("chain{i}")).await);
        }
        v
    };
    edge(&pool, ws, &c12, &model, e[0], e[1], 1.0).await;
    edge(&pool, ws, &c23, &model, e[1], e[2], 1.0).await;
    edge(&pool, ws, &c34, &model, e[2], e[3], 1.0).await;
    edge(&pool, ws, &c45, &model, e[3], e[4], 1.0).await;

    let seeds = vec![SeedChunk {
        content_hash: c12,
        rank: 1,
    }];
    let hits = graph_search::local_search_chunks(&pool, ws, &seeds, 50)
        .await
        .unwrap();

    assert_eq!(
        graph_search::MAX_HOPS,
        2,
        "this test is written against a cap of 2"
    );
    let h23 = hits
        .iter()
        .find(|h| h.page_id == p23)
        .expect("hop 1 must be reached");
    assert_eq!(h23.hops, 1);
    let h34 = hits
        .iter()
        .find(|h| h.page_id == p34)
        .expect("hop 2 must be reached");
    assert_eq!(h34.hops, 2);
    assert!(
        !has(&hits, p45),
        "hop 3 is beyond MAX_HOPS and must NOT be returned — an uncapped traversal on a \
         dense graph is a self-DoS"
    );
}

// ---------------------------------------------------------------------------
// Liveness — the shared `clusterable_edges_cte!` macro
// ---------------------------------------------------------------------------

/// An edge extracted from a chunk that only an ARCHIVED page still references is
/// not clusterable, so the traversal must not walk THROUGH it.
///
/// # Where the failure is rigged, and why it has to be here
///
/// The naive version of this test — archive the destination page and assert it
/// is absent — is VACUOUS: the final projection's own `p.archived_at IS NULL`
/// would exclude it even with the macro's liveness deleted, so the test would
/// pass by early return and look correct forever.
///
/// So the archived page is placed MID-CHAIN and the assertion is about a page
/// BEYOND it that is perfectly live:
///
/// ```text
///   seed chunk (live ALPHA) — E_A ── E_B ── E_C ── E_D
///                                     ^      ^
///                        edge from a chunk   edge from a chunk
///                        only ARCHIVED holds  only live GAMMA holds
/// ```
///
/// GAMMA is live, in this workspace, and two hops out — well inside the cap. It
/// is absent for exactly one reason: the dead link in the middle was not
/// traversable.
///
/// MECHANISM PROTECTED: `p.archived_at IS NULL` inside `clusterable_edges_cte!`.
/// Delete it and GAMMA becomes reachable and this test fails.
#[tokio::test]
async fn an_archived_pages_edge_cannot_be_traversed_through() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = unique_model();

    let (_, seed_chunk) = page(&pool, ws, "Alpha", "the live starting point").await;
    let (dead, dead_chunk) = page(&pool, ws, "Dead", "content the user deleted").await;
    let (gamma, gamma_chunk) =
        page(&pool, ws, "Gamma", "a perfectly live page beyond the gap").await;

    let ea = entity(&pool, ws, "ea").await;
    let eb = entity(&pool, ws, "eb").await;
    let ec = entity(&pool, ws, "ec").await;
    let ed = entity(&pool, ws, "ed").await;
    edge(&pool, ws, &seed_chunk, &model, ea, eb, 1.0).await;
    edge(&pool, ws, &dead_chunk, &model, eb, ec, 1.0).await;
    edge(&pool, ws, &gamma_chunk, &model, ec, ed, 1.0).await;

    let seeds = vec![SeedChunk {
        content_hash: seed_chunk.clone(),
        rank: 1,
    }];

    // Before archiving, GAMMA is reachable — so its later absence is caused by
    // the archive and not by a broken fixture.
    let before = graph_search::local_search_chunks(&pool, ws, &seeds, 50)
        .await
        .unwrap();
    assert!(
        has(&before, gamma),
        "fixture premise: GAMMA is reachable while the chain is live"
    );

    archive(&pool, dead).await;

    let after = graph_search::local_search_chunks(&pool, ws, &seeds, 50)
        .await
        .unwrap();
    assert!(
        !has(&after, gamma),
        "an archived page's edge must not be a bridge: GAMMA is live but is only \
         reachable through content the user deleted"
    );
}

/// A chunk is CONTENT-ADDRESSED and globally shared, so the same text can sit on
/// a live page and an archived one at once. The archived page must never be
/// returned even though its chunk is perfectly live.
///
/// The failure is rigged at the OUTPUT projection, which is the only mechanism
/// that can catch this case — the macro's liveness cannot, because the chunk IS
/// referenced by a live page.
///
/// MECHANISM PROTECTED: `p.archived_at IS NULL` in the final `resolved` CTE.
/// Delete it and the archived page joins the results.
#[tokio::test]
async fn an_archived_page_sharing_a_live_chunk_is_still_never_returned() {
    let pool = pool().await;
    let ws = workspace(&pool).await;

    let (live, shared) = page(&pool, ws, "Live", "text that exists on two pages").await;
    let (dead, _) = page(&pool, ws, "Dead", "placeholder").await;
    // Same content hash on both pages — the shared-chunk case M1b makes routine.
    noted_db::chunks::set_page_chunks(&pool, dead, &[shared.clone()])
        .await
        .unwrap();
    archive(&pool, dead).await;

    let seeds = vec![SeedChunk {
        content_hash: shared,
        rank: 1,
    }];
    let hits = graph_search::local_search_chunks(&pool, ws, &seeds, 50)
        .await
        .unwrap();

    assert!(
        has(&hits, live),
        "the live page holding the seed chunk must be returned"
    );
    assert!(
        !has(&hits, dead),
        "an archived page must never be returned, even when its chunk is alive elsewhere"
    );
}

// ---------------------------------------------------------------------------
// Tenancy — both ends
// ---------------------------------------------------------------------------

/// Another workspace's EDGES must never be traversed.
///
/// # Where the failure is rigged
///
/// The obvious fixture — B's graph leading to B's page — is VACUOUS: the output
/// projection's `p.workspace_id = $1` would drop B's page anyway, so deleting
/// the macro's `e.workspace_id = $1` would break nothing visible.
///
/// So the leak is made to land on a page A OWNS. Both workspaces hold
/// byte-identical text (which M1b makes ONE shared `chunks` row), and only B has
/// a graph:
///
/// ```text
///   seed chunk   — shared by A's "A-seed" and B's "B-seed"
///   quiet chunk  — shared by A's "A-quiet" and B's "B-quiet"
///   B's graph:  EB1 ──(seed chunk)── EB2 ──(quiet chunk)── EB3
///   A's graph:  none
/// ```
///
/// Searching A therefore must return A-seed alone. If B's edges were traversable
/// from A, the traversal would reach the quiet chunk and the projection would
/// happily resolve it to A-QUIET — a live page of A's, surfaced on the strength
/// of another tenant's graph. That is the M2a trap in its original shape: a
/// global content key gating a per-tenant decision.
///
/// MECHANISM PROTECTED: `e.workspace_id = $1` inside `clusterable_edges_cte!`.
#[tokio::test]
async fn another_workspaces_edges_are_never_traversed() {
    let pool = pool().await;
    let ws_a = workspace(&pool).await;
    let ws_b = workspace(&pool).await;
    let model = unique_model();

    let (a_seed, seed_chunk) = page(&pool, ws_a, "A-seed", "shared seed sentence").await;
    let (a_quiet, quiet_chunk) = page(&pool, ws_a, "A-quiet", "shared quiet sentence").await;

    // B's pages carry the SAME chunks — content addressing, not a contrivance.
    let (b_seed, _) = page(&pool, ws_b, "B-seed", "placeholder").await;
    let (b_quiet, _) = page(&pool, ws_b, "B-quiet", "placeholder").await;
    noted_db::chunks::set_page_chunks(&pool, b_seed, &[seed_chunk.clone()])
        .await
        .unwrap();
    noted_db::chunks::set_page_chunks(&pool, b_quiet, &[quiet_chunk.clone()])
        .await
        .unwrap();

    let eb1 = entity(&pool, ws_b, "eb1").await;
    let eb2 = entity(&pool, ws_b, "eb2").await;
    let eb3 = entity(&pool, ws_b, "eb3").await;
    edge(&pool, ws_b, &seed_chunk, &model, eb1, eb2, 1.0).await;
    edge(&pool, ws_b, &quiet_chunk, &model, eb2, eb3, 1.0).await;

    let a_edges: i64 = sqlx::query_scalar("SELECT count(*) FROM edges WHERE workspace_id = $1")
        .bind(ws_a)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        a_edges, 0,
        "fixture premise: workspace A has no graph of its own"
    );

    let seeds = vec![SeedChunk {
        content_hash: seed_chunk,
        rank: 1,
    }];
    let hits = graph_search::local_search_chunks(&pool, ws_a, &seeds, 50)
        .await
        .unwrap();

    assert!(has(&hits, a_seed), "A's own seed page must be returned");
    assert!(
        !has(&hits, a_quiet),
        "A-QUIET is reachable ONLY through workspace B's edges — surfacing it would mean \
         one tenant's graph is steering another tenant's retrieval"
    );
    // Sanity: B's own pages are unreachable too, via the projection.
    assert!(!has(&hits, b_seed) && !has(&hits, b_quiet));
}

/// A seed chunk is a caller-supplied, GLOBAL content hash, so resolving it to
/// pages must be workspace-scoped independently of the graph.
///
/// The failure is rigged at the projection because that is the only mechanism in
/// play: this fixture has no edges at all, so the macro cannot be what saves it.
///
/// MECHANISM PROTECTED: `p.workspace_id = $1` in the final `resolved` CTE.
#[tokio::test]
async fn seed_chunks_never_resolve_to_another_workspaces_page() {
    let pool = pool().await;
    let ws_a = workspace(&pool).await;
    let ws_b = workspace(&pool).await;

    let (a_page, shared) = page(&pool, ws_a, "A", "identical text in two tenants").await;
    let (b_page, _) = page(&pool, ws_b, "B", "placeholder").await;
    noted_db::chunks::set_page_chunks(&pool, b_page, &[shared.clone()])
        .await
        .unwrap();

    let seeds = vec![SeedChunk {
        content_hash: shared,
        rank: 1,
    }];
    let hits = graph_search::local_search_chunks(&pool, ws_a, &seeds, 50)
        .await
        .unwrap();

    assert!(has(&hits, a_page));
    assert!(
        !has(&hits, b_page),
        "a shared content hash must never leak another tenant's page into results"
    );
}

// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------

/// The three monotonicities the blend guarantees, each asserted with the other
/// two factors held fixed, plus the two places where the blend is deliberately
/// NOT lexicographic.
///
/// One fixture, all comparisons by RELATIVE POSITION — never by score, so the
/// constants can be retuned without rewriting the test.
///
/// ```text
///   S1 (seed rank 1) — A1 ── A2 ──(w 1.0)── A3 ──(w 1.0)── A5     evidence PH, PD
///                             └───(w 0.6)── A4                     evidence PL
///                             └───(w 0.1)── A6                     evidence PVL
///                             └───(w 0.1)── A7 ──(w 1.0)── A8      evidence PVL2, PB
///   S2 (seed rank 100) — B1 ── B2 ──(w 1.0)── B3                   evidence PR
/// ```
///
/// MECHANISMS PROTECTED, one per factor, each with its own killing assertion:
///   * `power(HOP_DECAY, hops)` — killed by `PL` (1 hop, weight 0.6) outranking
///     `PD` (2 hops, weight 1.0). Without the decay `PD` scores strictly higher.
///   * `* bottleneck` — killed by `PD` (2 hops, weight 1.0) outranking `PVL`
///     (1 hop, weight 0.1). Without the weight factor `PVL` wins on hops alone.
///   * `1/(RRF_K + seed_rank)` — killed by `PD` (2 hops, seed rank 1)
///     outranking `PR` (1 hop, seed rank 100). Without the seed term `PR` wins.
///   * `LEAST(...)` — the BOTTLENECK, as opposed to just the last edge's weight.
///     Killed by `PVL` (1 hop, weight 0.1) outranking `PB` (2 hops, over a
///     0.1 edge then a 1.0 edge). Without `LEAST` the path's weight is the last
///     edge's 1.0, `PB` scores ten times higher, and the order flips. This
///     assertion was added because the mutation run found `LEAST` unobservable.
///
/// Each kill is a strict inequality with no ties, deliberately: an assertion
/// that a mutation turns into a TIE dies only on a coin flip, which is not
/// evidence.
#[tokio::test]
async fn ranking_is_monotone_in_hops_seed_rank_and_edge_weight() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = unique_model();

    let (s1, cs1) = page(&pool, ws, "S1", "best seed").await;
    let (_s2, cs2) = page(&pool, ws, "S2", "poor seed").await;
    let (ph, ch) = page(&pool, ws, "PH", "one hop, full weight").await;
    let (pl, cl) = page(&pool, ws, "PL", "one hop, weight six tenths").await;
    let (pvl, cvl) = page(&pool, ws, "PVL", "one hop, weight one tenth").await;
    let (pd, cd) = page(&pool, ws, "PD", "two hops, full weight").await;
    let (pr, cr) = page(&pool, ws, "PR", "one hop from the poor seed").await;
    let (_pvl2, cvl2) = page(&pool, ws, "PVL2", "one hop over a weak edge").await;
    let (pb, cb) = page(&pool, ws, "PB", "two hops, weak edge first then strong").await;

    let a1 = entity(&pool, ws, "a1").await;
    let a2 = entity(&pool, ws, "a2").await;
    let a3 = entity(&pool, ws, "a3").await;
    let a4 = entity(&pool, ws, "a4").await;
    let a5 = entity(&pool, ws, "a5").await;
    let a6 = entity(&pool, ws, "a6").await;
    let b1 = entity(&pool, ws, "b1").await;
    let b2 = entity(&pool, ws, "b2").await;
    let b3 = entity(&pool, ws, "b3").await;
    let a7 = entity(&pool, ws, "a7").await;
    let a8 = entity(&pool, ws, "a8").await;

    edge(&pool, ws, &cs1, &model, a1, a2, 1.0).await;
    edge(&pool, ws, &ch, &model, a2, a3, 1.0).await;
    edge(&pool, ws, &cl, &model, a2, a4, 0.6).await;
    edge(&pool, ws, &cvl, &model, a2, a6, 0.1).await;
    edge(&pool, ws, &cd, &model, a3, a5, 1.0).await;
    edge(&pool, ws, &cs2, &model, b1, b2, 1.0).await;
    edge(&pool, ws, &cr, &model, b2, b3, 1.0).await;
    // The bottleneck path: weak edge FIRST, strong edge second.
    edge(&pool, ws, &cvl2, &model, a2, a7, 0.1).await;
    edge(&pool, ws, &cb, &model, a7, a8, 1.0).await;

    let seeds = vec![
        SeedChunk {
            content_hash: cs1,
            rank: 1,
        },
        SeedChunk {
            content_hash: cs2,
            rank: 100,
        },
    ];
    let hits = graph_search::local_search_chunks(&pool, ws, &seeds, 50)
        .await
        .unwrap();

    let p = |id: Uuid, what: &str| pos(&hits, id).unwrap_or_else(|| panic!("missing: {what}"));
    let (s1p, php, plp, pvlp, pdp, prp, pbp) = (
        p(s1, "S1"),
        p(ph, "PH"),
        p(pl, "PL"),
        p(pvl, "PVL"),
        p(pd, "PD"),
        p(pr, "PR"),
        p(pb, "PB"),
    );

    // Closer is stronger (seed rank and weight equal).
    assert!(s1p < php, "a seed must outrank a 1-hop hit off it");
    assert!(
        php < pdp,
        "a 1-hop hit must outrank a 2-hop hit on the same path"
    );
    // A better seed is stronger (hops and weight equal).
    assert!(
        php < prp,
        "1 hop off seed rank 1 must outrank 1 hop off seed rank 100"
    );
    // A stronger path is stronger (hops and seed rank equal).
    assert!(php < plp, "a full-weight hop must outrank a 0.6-weight hop");
    assert!(plp < pvlp, "a 0.6-weight hop must outrank a 0.1-weight hop");

    // The three factors each genuinely move the ordering — these are the
    // assertions that die when the corresponding term is deleted.
    assert!(
        plp < pdp,
        "hop decay: a 0.6-weight 1-hop beats a full-weight 2-hop"
    );
    assert!(
        pdp < pvlp,
        "edge weight: a full-weight 2-hop beats a 0.1-weight 1-hop"
    );
    assert!(
        pdp < prp,
        "seed rank: a 2-hop off the best seed beats a 1-hop off seed rank 100"
    );
    assert!(
        pvlp < pbp,
        "bottleneck: a path's weight is its WEAKEST link, not its last one — PB crosses a \
         0.1 edge before its 1.0 edge and must not outrank a single 0.1 hop"
    );
}

/// The same chunk can legitimately appear TWICE on one page (identical text in
/// two blocks is one content-addressed `chunks` row referenced at two
/// `chunk_index`es). A citation list that named it twice would be a duplicate in
/// the user's face and would also spend two slots of the limit on one fact.
///
/// MECHANISM PROTECTED: `DISTINCT ON (p.id, b.content_hash)` in `resolved`.
/// Added after the mutation run found that predicate unobservable — every other
/// fixture gives each page exactly one chunk, so nothing reached it.
#[tokio::test]
async fn a_chunk_appearing_twice_on_one_page_is_returned_once() {
    let pool = pool().await;
    let ws = workspace(&pool).await;

    let (p1, hash) = page(&pool, ws, "Doubled", "a line that appears twice").await;
    noted_db::chunks::set_page_chunks(&pool, p1, &[hash.clone(), hash.clone()])
        .await
        .unwrap();
    let refs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM page_chunks WHERE page_id = $1 AND content_hash = $2",
    )
    .bind(p1)
    .bind(&hash)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        refs, 2,
        "fixture premise: the page references the chunk twice"
    );

    let seeds = vec![SeedChunk {
        content_hash: hash,
        rank: 1,
    }];
    let hits = graph_search::local_search_chunks(&pool, ws, &seeds, 50)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "one page, one chunk, one citation");
    assert_eq!(hits[0].page_id, p1);
}

// ---------------------------------------------------------------------------
// The "why these results" surface
// ---------------------------------------------------------------------------

/// `seed_entities` is what the UI shows as "why these results", so it must name
/// both ends of a seeded edge, and must obey the same liveness and tenancy rules
/// the traversal does.
///
/// # Where the failures are rigged
///
/// Both negatives are rigged ON the seed set, which is the only place they can
/// reach the liveness macro. The first draft of this test put the archived
/// edge's chunk OUTSIDE the seed list — so `ce.source_chunk_hash = ANY($2)`
/// excluded it regardless, and deleting the macro's `archived_at IS NULL`
/// changed nothing. The mutation run caught that; the archived chunk is now a
/// SEED whose only page is archived.
///
/// MECHANISMS PROTECTED (two, and they are checked independently below):
///   * `p.archived_at IS NULL` in `clusterable_edges_cte!` — a seed chunk that
///     only an archived page still holds anchors NOTHING.
///   * `e.workspace_id = $1` in `clusterable_edges_cte!` together with
///     `en.workspace_id = $1` on the join — another tenant's edge over the same
///     shared chunk names none of this workspace's seed entities.
#[tokio::test]
async fn seed_entities_names_both_ends_and_nothing_dead_or_foreign() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let other = workspace(&pool).await;
    let model = unique_model();

    let (_, seed_chunk) = page(&pool, ws, "Seed", "the live seed").await;
    let (dead, dead_chunk) = page(&pool, ws, "Dead", "archived content").await;

    let left = entity(&pool, ws, "left").await;
    let right = entity(&pool, ws, "right").await;
    let buried = entity(&pool, ws, "buried").await;
    let buried2 = entity(&pool, ws, "buried2").await;
    edge(&pool, ws, &seed_chunk, &model, left, right, 1.0).await;
    edge(&pool, ws, &dead_chunk, &model, buried, buried2, 1.0).await;

    // Another tenant holding the SAME chunk text, with its own entities on it.
    let (other_page, _) = page(&pool, other, "Other", "placeholder").await;
    noted_db::chunks::set_page_chunks(&pool, other_page, &[seed_chunk.clone()])
        .await
        .unwrap();
    let foreign = entity(&pool, other, "foreign").await;
    let foreign2 = entity(&pool, other, "foreign2").await;
    edge(&pool, other, &seed_chunk, &model, foreign, foreign2, 1.0).await;

    archive(&pool, dead).await;

    // BOTH chunks are seeds. The archived one must contribute nothing — that is
    // the assertion the macro's liveness has to earn.
    let seeds = vec![
        SeedChunk {
            content_hash: seed_chunk,
            rank: 1,
        },
        SeedChunk {
            content_hash: dead_chunk,
            rank: 2,
        },
    ];
    let found = graph_search::seed_entities(&pool, ws, &seeds)
        .await
        .unwrap();
    let ids: Vec<Uuid> = found.iter().map(|(id, _)| *id).collect();

    assert!(
        ids.contains(&left),
        "the source end of a seeded edge is a seed entity"
    );
    assert!(
        ids.contains(&right),
        "so is the target end — direction is a phrasing artefact"
    );
    assert!(
        !ids.contains(&buried),
        "a seed chunk only an archived page holds anchors nothing"
    );
    assert!(!ids.contains(&buried2));
    assert!(
        !ids.contains(&foreign),
        "another tenant's entity must never appear"
    );
    assert!(!ids.contains(&foreign2));
    assert_eq!(ids.len(), 2);
}

/// No seeds is not an error and not "everything" — it is an empty result. Both
/// entry points short-circuit before touching the database.
#[tokio::test]
async fn no_seeds_returns_nothing() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    assert!(
        graph_search::local_search_chunks(&pool, ws, &[], 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        graph_search::seed_entities(&pool, ws, &[])
            .await
            .unwrap()
            .is_empty()
    );
}

/// `limit` is clamped in the REPOSITORY, not in a handler, so every caller
/// inherits the cap — the same reason and the same shape as
/// `pages::MAX_RECENT_LIMIT`. A zero or negative limit is caller error, not a
/// request for everything.
#[tokio::test]
async fn the_result_limit_is_clamped_in_the_repository() {
    let pool = pool().await;
    let ws = workspace(&pool).await;

    let mut seeds = Vec::new();
    for i in 0..3 {
        let (_, h) = page(&pool, ws, &format!("L{i}"), &format!("limit fixture {i}")).await;
        seeds.push(SeedChunk {
            content_hash: h,
            rank: i + 1,
        });
    }

    let zero = graph_search::local_search_chunks(&pool, ws, &seeds, 0)
        .await
        .unwrap();
    assert_eq!(
        zero.len(),
        1,
        "a zero limit is treated as 1, never as unlimited"
    );

    let huge = graph_search::local_search_chunks(&pool, ws, &seeds, 10_000)
        .await
        .unwrap();
    assert_eq!(huge.len(), 3, "and an absurd limit is capped, not obeyed");
    assert!(graph_search::MAX_LOCAL_LIMIT >= 3);
}

//! M2c Task 4 — THE WIRING GAP.
//!
//! M2b shipped `CommunityWorker` with no production caller. These tests are the
//! ones that fail if that is ever true again: everything here reaches the
//! community layer WITHOUT naming it, by driving the extraction path only.
//!
//! Every fixture is scoped to its own freshly-created workspace, and every
//! extractor carries a per-test `model_id`, because this crate's tests run
//! against a shared dev database. Nothing here asserts anything instance-wide
//! (see the note in `tests/materialize.rs` about an instance-wide assertion with
//! a `LIMIT`). Chunk text carries a lowercase per-test `run` marker so that
//! content-addressed chunks — which ARE globally shared (M1b) — never collide
//! between tests and fan an extraction into another test's workspace. Lowercase,
//! because `StubExtractor` treats capitalised words as entities and the marker
//! must not become one.
use noted_db::community;
use noted_index::extract::{ExtractError, Extraction, ExtractionProvider, StubExtractor};
use noted_index::extract_worker::ExtractWorker;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

// ---------------------------------------------------------------- fixtures --

/// `StubExtractor`'s behaviour under a per-test `model_id`, so the extraction
/// queue (a set difference keyed on `model_id`) only ever contains this test's
/// own chunks.
struct TaggedStub(String);

#[async_trait::async_trait]
impl ExtractionProvider for TaggedStub {
    fn model_id(&self) -> &str {
        &self.0
    }
    async fn extract(&self, text: &str) -> Result<Extraction, ExtractError> {
        StubExtractor::new().extract(text).await
    }
}

async fn connect() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

async fn workspace(pool: &noted_db::PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('wiring-test') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn page(pool: &noted_db::PgPool, ws: Uuid) -> Uuid {
    sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
        .bind(ws)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A page's chunk list, accumulated across calls — `set_page_chunks` replaces
/// the whole list, so a fixture that adds a second chunk has to re-send the
/// first.
#[derive(Default)]
struct PageChunks(Vec<String>);

impl PageChunks {
    async fn add(&mut self, pool: &noted_db::PgPool, pg: Uuid, hash: &str, text: &str) {
        noted_db::chunks::upsert(pool, &[(hash.to_string(), text.to_string(), 10)])
            .await
            .unwrap();
        if !self.0.contains(&hash.to_string()) {
            self.0.push(hash.to_string());
        }
        noted_db::chunks::set_page_chunks(pool, pg, &self.0)
            .await
            .unwrap();
    }
}

/// Seed a `n`-entity clique named `prefix1..prefixN` directly through the
/// repository, on ONE live chunk.
///
/// The chunk's edges are written under `model` — the SAME model the test's
/// extractor uses — precisely so `replace_chunk_edges` writes its
/// `chunk_extractions` marker and this chunk is therefore ALREADY EXTRACTED as
/// far as `pending_extraction` is concerned. Without that, the seeded chunk
/// would sit in the queue and the drain under test would extract it too,
/// inventing entities and churn the test never asked for.
async fn seed_clique(
    pool: &noted_db::PgPool,
    ws: Uuid,
    pg: Uuid,
    chunks: &mut PageChunks,
    run: &str,
    model: &str,
    prefix: &str,
    n: usize,
) -> HashMap<String, Uuid> {
    let hash = format!("wire-{run}-{prefix}");
    chunks
        .add(pool, pg, &hash, &format!("seeded clique {run} {prefix}"))
        .await;

    let mut ids = HashMap::new();
    for i in 1..=n {
        let name = format!("{prefix}{i}");
        let id = noted_db::graph::resolve_entity(pool, ws, &name, Some("CONCEPT"), None)
            .await
            .unwrap();
        ids.insert(name, id);
    }

    let mut tuples = Vec::new();
    for i in 1..=n {
        for j in (i + 1)..=n {
            tuples.push((
                ids[&format!("{prefix}{i}")],
                ids[&format!("{prefix}{j}")],
                "mentions_with".to_string(),
                1.0f32,
            ));
        }
    }
    noted_db::graph::replace_chunk_edges(pool, ws, &hash, model, &tuples)
        .await
        .unwrap();

    ids
}

/// `entity name -> community id` for a workspace.
async fn memberships(pool: &noted_db::PgPool, ws: Uuid) -> HashMap<String, Uuid> {
    let rows: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT e.name, cm.community_id
         FROM communities c
         JOIN community_members cm ON cm.community_id = c.id
         JOIN entities e           ON e.id = cm.entity_id
         WHERE c.workspace_id = $1",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter().collect()
}

// ------------------------------------------------------------ the headline --

/// THE HEADLINE, and stated with the honesty the plan demands.
///
/// A page is written the way the product writes one — blocks, then
/// `materialize::rechunk_page`, which is exactly what `noted-server`'s debounced
/// projection calls — and then the indexer runs. Nothing in this test names
/// `CommunityWorker`, `community::*`, or the louvain module, and yet the
/// workspace ends up with a clustered graph.
///
/// **What this does NOT claim:** that it happens with no human running a binary.
/// It does not. `ExtractWorker::drain` below is the CLI's work, and nothing in
/// `noted-server` calls it — the server projects and rechunks and stops there.
/// The honest statement of the closed gap is: *once the indexer runs at all, the
/// graph and its communities follow from the same pass, with no separate
/// community step for anyone to forget.* Server-side automation is a deliberate
/// non-goal of this task (see the report).
///
/// MECHANISM: the `on_edges_changed` call in `ExtractWorker::process_batch`.
/// Delete it and no community row is ever written by any production path.
#[tokio::test]
async fn writing_a_page_and_running_the_indexer_clusters_the_graph() {
    let pool = connect().await;
    let run = Uuid::new_v4().simple().to_string();
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let provider = Arc::new(TaggedStub(format!("wiring-headline-{run}")));

    // A page is written: blocks land, the projection rechunks. This is the
    // server's path verbatim (routes/sync.rs::project_page).
    for (i, text) in [
        format!("Ratchet Pawl escapement notes {run}"),
        format!("Pawl Detent spring notes {run}"),
    ]
    .iter()
    .enumerate()
    {
        sqlx::query(
            "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
             VALUES ($1, $2, 'paragraph', $3, md5($3))",
        )
        .bind(pg)
        .bind(i as i32)
        .bind(text)
        .execute(&pool)
        .await
        .unwrap();
    }
    let chunks = noted_index::materialize::rechunk_page(&pool, pg).await.unwrap();
    assert!(
        chunks > 0,
        "premise: the page must actually have produced chunks, or the rest of this test is \
         asserting over an empty pipeline"
    );

    // No community assertion is worth anything unless the workspace starts
    // without one.
    assert!(
        memberships(&pool, ws).await.is_empty(),
        "premise: a freshly created workspace must have no communities"
    );

    // The indexer's extraction pass. THE ONLY WORKER THIS TEST CONSTRUCTS.
    let extracted = ExtractWorker::new_scoped(pool.clone(), provider, ws)
        .drain()
        .await
        .unwrap();
    assert!(extracted > 0, "premise: the drain must have extracted something");

    let members = memberships(&pool, ws).await;
    assert!(
        !members.is_empty(),
        "extracting a page's chunks must leave the workspace with a clustered graph — this is \
         the wiring gap M2b left open: CommunityWorker had no production caller at all"
    );
    for name in ["ratchet", "pawl", "detent"] {
        assert!(
            members.contains_key(name),
            "entity '{name}' was extracted from the page but never made it into a community \
             ({members:?})"
        );
    }
}

// --------------------------------------------------- the affected-set claim --

/// `hot_reassign` applies moves IN SEQUENCE, each reading the last one's state,
/// so handing it the whole graph cascades transitively and merges distinct
/// communities (documented at length on the function). This proves the
/// extraction path hands it the entities the chunk actually touched — and, in
/// the same test and with the same fixture, that it hands it a NON-EMPTY set.
///
/// The fixture: two disconnected 7-cliques, cold-run into two communities, then
/// deliberately corrupted — `a1` is moved into B's community and `b1` into A's.
/// A chunk naming only `A1` is then extracted.
///
/// * `a1` must be pulled back into A's community: it was touched, so the hot
///   path saw it. Kills "affected is always empty".
/// * `b1` must STAY in A's community: it was not touched, so the hot path must
///   never have looked at it. Kills "affected is the whole workspace" — under
///   that mutation `b1`'s strongest neighbour (`b2`) drags it back to B.
///
/// 42 clusterable pairs and a 1-edge extraction keep churn (1) strictly below
/// the cold threshold (`ceil(0.05 * 43) = 3`), so no cold run fires to launder
/// either mutation into the right answer. That is load-bearing: a cold run
/// recomputes the exact partition and would make both mutations invisible.
#[tokio::test]
async fn only_the_entities_a_chunk_touched_are_reassigned() {
    let pool = connect().await;
    let run = Uuid::new_v4().simple().to_string();
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let model = format!("wiring-affected-{run}");
    let provider = Arc::new(TaggedStub(model.clone()));
    let mut chunks = PageChunks::default();

    let a = seed_clique(&pool, ws, pg, &mut chunks, &run, &model, "a", 7).await;
    let b = seed_clique(&pool, ws, pg, &mut chunks, &run, &model, "b", 7).await;

    // Two disconnected cliques: the cold path is what establishes the two
    // communities this test then perturbs.
    noted_index::community_worker::CommunityWorker::new(pool.clone(), ws)
        .cold_run()
        .await
        .unwrap();

    let before = memberships(&pool, ws).await;
    let ca = before["a3"];
    let cb = before["b3"];
    assert_ne!(
        ca, cb,
        "premise: the two disconnected cliques must cluster into two DIFFERENT communities, or \
         there is nothing for a cascade to merge"
    );
    assert_eq!(
        community::clusterable_edge_count(&pool, ws).await.unwrap(),
        42,
        "premise: the churn arithmetic below depends on this edge count"
    );

    // Corrupt the partition in both directions.
    community::reassign_entity(&pool, ws, a["a1"], cb)
        .await
        .unwrap();
    community::reassign_entity(&pool, ws, b["b1"], ca)
        .await
        .unwrap();

    // A chunk naming ONLY a1 (plus a brand-new entity, so the extraction has an
    // edge at all). `gnew` is capitalised so the stub sees it; `run` is not.
    chunks
        .add(
            &pool,
            pg,
            &format!("wire-{run}-touch"),
            &format!("A1 Gnew {run}"),
        )
        .await;

    // Captured BEFORE the drain, because this test ran its own `cold_run()`
    // above to establish the two communities — so `last_full_run.is_some()` is
    // already true here and asserting `false` after the drain could never hold.
    // The premise is that no cold run fires DURING THE DRAIN, and the only
    // honest way to say that is that the stamp did not move.
    let (_, run_before_drain) = community::churn(&pool, ws).await.unwrap();

    ExtractWorker::new_scoped(pool.clone(), provider, ws)
        .drain()
        .await
        .unwrap();

    let (churn, last_full_run) = community::churn(&pool, ws).await.unwrap();
    assert_eq!(
        (churn, last_full_run),
        (1, run_before_drain),
        "premise: exactly one edge of churn and NO cold run during the drain — a cold run would \
         recompute the exact partition and hide both mutations this test exists to catch"
    );

    let after = memberships(&pool, ws).await;
    assert_eq!(
        after["a1"], ca,
        "a1 was named by the extracted chunk, so the hot path must have seen it and pulled it \
         back into its own clique's community"
    );
    assert_eq!(
        after["b1"], ca,
        "b1 was NOT named by the extracted chunk, so the hot path must never have considered \
         it. If it moved back to B's community, the extraction path is handing hot_reassign \
         more than the edit touched — which cascades and merges distinct communities"
    );
}

// ---------------------------------------------------------- churn magnitude --

/// The churn the extraction path reports is the number of edges it wrote, not a
/// constant.
///
/// MECHANISM: the `edges_changed` argument to `on_edges_changed`. A 21-pair
/// workspace plus a 2-edge extraction puts the threshold at
/// `ceil(0.05 * 23) = 2` and the churn at exactly 2 — so the cold path fires.
/// Report `1` instead (or anything below the true count) and it does not, and
/// the workspace's partition is never corrected.
///
/// This is deliberately NOT covered by the headline test: there, the workspace
/// is small enough that the threshold is 1 and any positive churn fires a cold
/// run, so a constant would pass.
#[tokio::test]
async fn extraction_reports_the_number_of_edges_it_actually_wrote_as_churn() {
    let pool = connect().await;
    let run = Uuid::new_v4().simple().to_string();
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let model = format!("wiring-churn-{run}");
    let provider = Arc::new(TaggedStub(model.clone()));
    let mut chunks = PageChunks::default();

    seed_clique(&pool, ws, pg, &mut chunks, &run, &model, "p", 7).await;
    assert_eq!(
        community::clusterable_edge_count(&pool, ws).await.unwrap(),
        21,
        "premise: 21 pairs, so one more edge pair leaves the threshold at ceil(0.05 * 23) = 2"
    );
    assert_eq!(
        community::churn(&pool, ws).await.unwrap(),
        (0, None),
        "premise: the workspace has never been clustered and owes no churn"
    );

    // Three capitalised words => two edges from `StubExtractor`.
    chunks
        .add(
            &pool,
            pg,
            &format!("wire-{run}-churn"),
            &format!("Qaa Qab Qac {run}"),
        )
        .await;

    ExtractWorker::new_scoped(pool.clone(), provider, ws)
        .drain()
        .await
        .unwrap();

    let (churn, last_full_run) = community::churn(&pool, ws).await.unwrap();
    assert!(
        last_full_run.is_some(),
        "two edges of churn against a threshold of two must fire the cold path; reporting a \
         constant smaller than the real edge count leaves it below threshold forever"
    );
    assert_eq!(
        churn, 0,
        "and a completed cold run resets the counter (mark_full_run)"
    );
}

// ------------------------------------------------------------------ tenancy --

/// A chunk is content-addressed and GLOBALLY shared: two workspaces holding
/// byte-identical text share one row (M1b), and `process_batch` writes the
/// extraction into EVERY workspace whose live page references it — including
/// workspaces a scoped worker was not pointed at.
///
/// MECHANISM: the `on_edges_changed` call sits INSIDE that per-workspace loop
/// and is scoped to `*workspace_id`, not to the worker's own `workspace_id` and
/// not hoisted out of the loop. Move it out, or scope it to the worker, and the
/// second workspace gets edges with no community update — a graph that is
/// permanently one step behind for every tenant except the one that happened to
/// run the indexer.
#[tokio::test]
async fn every_workspace_sharing_a_chunk_has_its_communities_updated() {
    let pool = connect().await;
    let run = Uuid::new_v4().simple().to_string();
    let ws_a = workspace(&pool).await;
    let ws_b = workspace(&pool).await;
    let pg_a = page(&pool, ws_a).await;
    let pg_b = page(&pool, ws_b).await;
    let provider = Arc::new(TaggedStub(format!("wiring-shared-{run}")));

    // ONE content-addressed chunk, live on a page in each workspace.
    let hash = format!("wire-{run}-shared");
    let text = format!("Sextant Chronometer almanac {run}");
    noted_db::chunks::upsert(&pool, &[(hash.clone(), text, 10)])
        .await
        .unwrap();
    for pg in [pg_a, pg_b] {
        noted_db::chunks::set_page_chunks(&pool, pg, std::slice::from_ref(&hash))
            .await
            .unwrap();
    }

    // Scoped to A ONLY. The poll never looks at B.
    ExtractWorker::new_scoped(pool.clone(), provider, ws_a)
        .drain()
        .await
        .unwrap();

    for (label, ws) in [("A (the scoped workspace)", ws_a), ("B (the sharer)", ws_b)] {
        let members = memberships(&pool, ws).await;
        assert!(
            members.contains_key("sextant") && members.contains_key("chronometer"),
            "workspace {label} received the extraction's edges, so it must also have received \
             the community update that followed them ({members:?})"
        );
    }

    // And the two workspaces' communities are their own — a cross-tenant
    // membership would mean the community update was not workspace-scoped.
    let cross: i64 = sqlx::query_scalar(
        "SELECT count(*)
         FROM communities c
         JOIN community_members cm ON cm.community_id = c.id
         JOIN entities e           ON e.id = cm.entity_id
         WHERE c.workspace_id = $1 AND e.workspace_id <> $1",
    )
    .bind(ws_a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cross, 0, "no community may name another workspace's entity");
}

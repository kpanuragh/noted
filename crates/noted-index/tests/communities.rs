//! M2b Task 3 — the community worker's hot path, cold path, churn threshold,
//! and the convergence property that licenses the approximation.
//!
//! Every test scopes its fixtures to its OWN freshly-created workspace. That is
//! not tidiness: this crate's tests run against a shared dev database that has
//! previously accumulated hundreds of thousands of junk rows, and clustering is
//! a WHOLE-WORKSPACE operation — an unscoped one would cluster every entity any
//! earlier test ever created. Chunk text additionally carries a per-test
//! lowercase `run` marker so that content-addressed chunks (which ARE globally
//! shared — M1b) never collide between tests and fan an extraction into another
//! test's workspace. Lowercase, because `StubExtractor` treats capitalised words
//! as entities and the marker must not become one.
use noted_db::community;
use noted_index::community_worker::CommunityWorker;
use noted_index::extract::{ExtractError, Extraction, ExtractionProvider, StubExtractor};
use noted_index::extract_worker::ExtractWorker;
use noted_index::louvain::louvain;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use uuid::Uuid;

// ---------------------------------------------------------------- fixtures --

async fn connect() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

async fn workspace(pool: &noted_db::PgPool) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('community-worker-test') RETURNING id",
    )
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

/// Write a graph directly through the repository primitives, with one live
/// chunk on `page` supplying the provenance every clusterable edge needs.
///
/// `creation_order` controls the order `entities` rows are INSERTED, and
/// therefore the order of their `gen_random_uuid()` ids relative to each other.
/// That is the whole point of the parameter: it lets a test build the same graph
/// twice with two different id orders and prove the clusterer does not see them.
///
/// Used where a test needs an exact graph SHAPE. The convergence test uses the
/// real `ExtractWorker` instead, because there the production path IS the thing
/// under test.
async fn seed_graph(
    pool: &noted_db::PgPool,
    ws: Uuid,
    pg: Uuid,
    run: &str,
    tag: &str,
    creation_order: &[&str],
    edges: &[(&str, &str, f32)],
) -> HashMap<String, Uuid> {
    let hash = format!("cw-{run}-{tag}");
    noted_db::chunks::upsert(pool, &[(hash.clone(), format!("seed {run} {tag}"), 10)])
        .await
        .unwrap();

    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT content_hash FROM page_chunks WHERE page_id = $1 ORDER BY chunk_index",
    )
    .bind(pg)
    .fetch_all(pool)
    .await
    .unwrap();
    let mut all = existing;
    if !all.contains(&hash) {
        all.push(hash.clone());
    }
    noted_db::chunks::set_page_chunks(pool, pg, &all)
        .await
        .unwrap();

    let mut ids: HashMap<String, Uuid> = HashMap::new();
    // `creation_order` first, so the caller controls insertion (and therefore
    // uuid) order; then any endpoint it did not name, which is how a later call
    // attaches new edges to entities an earlier call already created.
    for name in creation_order
        .iter()
        .copied()
        .chain(edges.iter().flat_map(|(a, b, _)| [*a, *b]))
    {
        if ids.contains_key(name) {
            continue;
        }
        let id = noted_db::graph::resolve_entity(pool, ws, name, Some("CONCEPT"), None)
            .await
            .unwrap();
        ids.insert(name.to_string(), id);
    }

    let tuples: Vec<(Uuid, Uuid, String, f32)> = edges
        .iter()
        .map(|(a, b, w)| (ids[*a], ids[*b], "mentions_with".to_string(), *w))
        .collect();
    noted_db::graph::replace_chunk_edges(pool, ws, &hash, "seeded-model", &tuples)
        .await
        .unwrap();

    ids
}

/// The workspace's stored partition as sets of entity NAMES, canonically
/// sorted.
///
/// Names, not ids: two workspaces holding the same graph have entirely
/// different entity ids, so an id comparison could never express "these two
/// partitions are the same partition". Canonical form (members sorted, the
/// communities themselves sorted) rather than raw community ids, because two
/// partitions can be identical as set-partitions while carrying different
/// community rows — comparing ids reports spurious inequality. Same discipline
/// spec §7 requires of the Louvain tests.
async fn stored_partition(pool: &noted_db::PgPool, ws: Uuid) -> BTreeSet<Vec<String>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT c.id, e.name
         FROM communities c
         JOIN community_members cm ON cm.community_id = c.id
         JOIN entities e           ON e.id = cm.entity_id
         WHERE c.workspace_id = $1",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap();

    let mut by_community: HashMap<Uuid, Vec<String>> = HashMap::new();
    for (cid, name) in rows {
        by_community.entry(cid).or_default().push(name);
    }
    by_community
        .into_values()
        .map(|mut m| {
            m.sort();
            m
        })
        .collect()
}

/// The community (as a sorted name set) currently holding `name`, if any.
async fn community_of(pool: &noted_db::PgPool, ws: Uuid, name: &str) -> Option<Vec<String>> {
    let members: Vec<String> = sqlx::query_scalar(
        "SELECT e2.name
         FROM entities e1
         JOIN community_members cm1 ON cm1.entity_id = e1.id
         JOIN communities c         ON c.id = cm1.community_id AND c.workspace_id = $1
         JOIN community_members cm2 ON cm2.community_id = c.id
         JOIN entities e2           ON e2.id = cm2.entity_id
         WHERE e1.workspace_id = $1 AND e1.name = $2",
    )
    .bind(ws)
    .bind(name)
    .fetch_all(pool)
    .await
    .unwrap();

    if members.is_empty() {
        return None;
    }
    let mut m = members;
    m.sort();
    Some(m)
}

async fn archive(pool: &noted_db::PgPool, pg: Uuid) {
    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(pg)
        .execute(pool)
        .await
        .unwrap();
}

/// Two 4-cliques joined by a single bridge edge — a graph whose correct
/// partition is unambiguous, so a wrong clusterer or a wrong index mapping is
/// visible rather than merely different.
const TWO_CLIQUES: &[(&str, &str, f32)] = &[
    ("aa", "ab", 1.0),
    ("aa", "ac", 1.0),
    ("aa", "ad", 1.0),
    ("ab", "ac", 1.0),
    ("ab", "ad", 1.0),
    ("ac", "ad", 1.0),
    ("xa", "xb", 1.0),
    ("xa", "xc", 1.0),
    ("xa", "xd", 1.0),
    ("xb", "xc", 1.0),
    ("xb", "xd", 1.0),
    ("xc", "xd", 1.0),
    ("ad", "xa", 1.0),
];
const TWO_CLIQUE_NODES: &[&str] = &["aa", "ab", "ac", "ad", "xa", "xb", "xc", "xd"];

fn two_clique_partition() -> BTreeSet<Vec<String>> {
    BTreeSet::from([
        vec!["aa".into(), "ab".into(), "ac".into(), "ad".into()],
        vec!["xa".into(), "xb".into(), "xc".into(), "xd".into()],
    ])
}

// ------------------------------------------------------------------- tests --

/// The cold path end to end: read the clusterable graph, cluster it, and store
/// the result under the right entity ids.
///
/// Asserts the EXACT expected partition rather than "some partition exists".
/// Two disjoint 4-cliques joined by one edge have exactly one sensible
/// clustering, so this is simultaneously a correctness anchor for the wiring
/// (an index-mapping bug puts the right sets under the wrong names) and for the
/// clusterer's integration.
#[tokio::test]
async fn a_cold_run_stores_the_partition_of_the_live_graph() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();

    seed_graph(&pool, ws, pg, &run, "g", TWO_CLIQUE_NODES, TWO_CLIQUES).await;

    let n = CommunityWorker::new(pool.clone(), ws)
        .cold_run()
        .await
        .unwrap();

    assert_eq!(n, 2, "two 4-cliques joined by one edge are two communities");
    assert_eq!(
        stored_partition(&pool, ws).await,
        two_clique_partition(),
        "the stored partition must be the two cliques, under the right entity names"
    );
}

/// THE M2b-1 PREREQUISITE, in test form. Nothing in this system removes graph
/// nodes or edges: archiving a page (the product's delete) leaves its entities
/// and edges completely intact, and nothing will ever revisit them. Clustering
/// the raw tables would therefore put user-deleted content into communities,
/// which M2b-3 would summarise and M2c would retrieve.
///
/// The decision is to FILTER at clustering time rather than build a reaper — no
/// destructive path, and reversible if the page is un-archived. This test pins
/// that: an entity whose only edges come from an archived page is not
/// clusterable.
///
/// A live clique is present too, so the assertion cannot be satisfied by an
/// empty partition. A test whose expected value is "nothing" passes just as
/// happily when the whole feature is broken.
#[tokio::test]
async fn entities_reachable_only_from_an_archived_page_are_not_clustered() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let live_page = page(&pool, ws).await;
    let dead_page = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();

    let live: &[(&str, &str, f32)] = &[("aa", "ab", 1.0), ("aa", "ac", 1.0), ("ab", "ac", 1.0)];
    let dead: &[(&str, &str, f32)] = &[("za", "zb", 1.0), ("za", "zc", 1.0), ("zb", "zc", 1.0)];
    seed_graph(
        &pool,
        ws,
        live_page,
        &run,
        "live",
        &["aa", "ab", "ac"],
        live,
    )
    .await;
    seed_graph(
        &pool,
        ws,
        dead_page,
        &run,
        "dead",
        &["za", "zb", "zc"],
        dead,
    )
    .await;

    archive(&pool, dead_page).await;

    CommunityWorker::new(pool.clone(), ws)
        .cold_run()
        .await
        .unwrap();

    assert_eq!(
        stored_partition(&pool, ws).await,
        BTreeSet::from([vec!["aa".to_string(), "ab".to_string(), "ac".to_string()]]),
        "only the live clique may be clustered; the archived page's entities and edges still \
         exist in full and must not appear"
    );

    // And the rows really are still there — this is a filter, not a reaper, and
    // the un-archive path depends on it.
    let survivors: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM entities WHERE workspace_id = $1 AND name LIKE 'z%'",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        survivors, 3,
        "the archived page's entities must survive untouched — nothing here deletes graph nodes"
    );
}

/// Liveness is judged against the edge's OWN workspace's pages.
///
/// `source_chunk_hash` is a global, content-addressed key: two workspaces with
/// byte-identical text share one `chunks` row (M1b). An edge, by contrast,
/// belongs to exactly one workspace (0007). So a clusterable filter that asked
/// only "is any live page referencing this chunk?" would let workspace B's live
/// page keep workspace A's DELETED content in A's communities — one tenant's
/// data resurrecting another tenant's.
///
/// This is the M2a standing rule in its exact original shape (a global content
/// key gating a per-tenant decision), which is why it gets its own test rather
/// than being assumed to fall out of the archived-page one above.
#[tokio::test]
async fn an_edge_is_dead_when_its_own_workspaces_page_is_archived_even_if_another_workspace_shares_the_chunk(
) {
    let pool = connect().await;
    let ws_a = workspace(&pool).await;
    let ws_b = workspace(&pool).await;
    let page_a = page(&pool, ws_a).await;
    let page_b = page(&pool, ws_b).await;
    let keep_a = page(&pool, ws_a).await;
    let run = Uuid::new_v4().simple().to_string();

    let shared: &[(&str, &str, f32)] = &[("sa", "sb", 1.0), ("sa", "sc", 1.0), ("sb", "sc", 1.0)];
    // The SAME chunk hash in both workspaces (`tag` is identical), which is
    // exactly what content addressing produces for identical text.
    seed_graph(
        &pool,
        ws_a,
        page_a,
        &run,
        "shared",
        &["sa", "sb", "sc"],
        shared,
    )
    .await;
    seed_graph(
        &pool,
        ws_b,
        page_b,
        &run,
        "shared",
        &["sa", "sb", "sc"],
        shared,
    )
    .await;
    // A's own unrelated live clique, so the expected partition is not empty.
    let keep: &[(&str, &str, f32)] = &[("ka", "kb", 1.0), ("ka", "kc", 1.0), ("kb", "kc", 1.0)];
    seed_graph(&pool, ws_a, keep_a, &run, "keep", &["ka", "kb", "kc"], keep).await;

    // A deletes its page. B still has a live page over the very same chunk.
    archive(&pool, page_a).await;

    CommunityWorker::new(pool.clone(), ws_a)
        .cold_run()
        .await
        .unwrap();

    assert_eq!(
        stored_partition(&pool, ws_a).await,
        BTreeSet::from([vec!["ka".to_string(), "kb".to_string(), "kc".to_string()]]),
        "workspace A archived the only page referencing that chunk, so those edges are dead FOR A \
         — workspace B's live page over the same content-addressed chunk must not keep them alive"
    );
}

/// The hot path and the cold path must share ONE definition of clusterable.
///
/// If they disagree, the convergence property compares two different graphs and
/// fails in ways indistinguishable from clusterer non-determinism. Here the
/// disagreement is made observable directly: `orphan` has exactly one edge, and
/// it comes from an archived page. The cold path cannot see it, so the hot path
/// must not either — an unfiltered neighbour lookup would happily find `aa`,
/// read its community, and file `orphan` into a community it has no live
/// connection to.
#[tokio::test]
async fn the_hot_path_will_not_route_an_entity_through_a_dead_edge() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let live_page = page(&pool, ws).await;
    let dead_page = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();

    let live: &[(&str, &str, f32)] = &[("aa", "ab", 1.0), ("aa", "ac", 1.0), ("ab", "ac", 1.0)];
    let ids = seed_graph(
        &pool,
        ws,
        live_page,
        &run,
        "live",
        &["aa", "ab", "ac"],
        live,
    )
    .await;
    let worker = CommunityWorker::new(pool.clone(), ws);
    worker.cold_run().await.unwrap();
    assert!(
        community_of(&pool, ws, "aa").await.is_some(),
        "sanity: the live clique must be clustered before the hot path is asked anything"
    );

    // `orphan`'s only edge is on a page that is then archived.
    let dead: &[(&str, &str, f32)] = &[("orphan", "aa", 1.0)];
    seed_graph(&pool, ws, dead_page, &run, "dead", &["orphan"], dead).await;
    let _ = ids;
    archive(&pool, dead_page).await;

    let orphan_id: Uuid =
        sqlx::query_scalar("SELECT id FROM entities WHERE workspace_id = $1 AND name = 'orphan'")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .unwrap();

    let moved = worker.hot_reassign(&[orphan_id]).await.unwrap();

    assert_eq!(
        moved, 0,
        "an entity whose only edge is dead has no clusterable neighbour and must not be moved"
    );
    assert_eq!(
        community_of(&pool, ws, "orphan").await,
        None,
        "the hot path must not file an entity into a community it reaches only through an edge \
         the cold path cannot see"
    );
}

/// Canonical node order is `entities.name`, NEVER `entities.id`.
///
/// `id` is `gen_random_uuid()` — random per insert. Id order is therefore
/// stable within one database but not across a rebuild, so ordering by it would
/// make the convergence property fail spuriously (two structurally identical
/// graphs handed to the clusterer under two different node numberings).
/// `UNIQUE (workspace_id, name)` makes `name` the stable natural key.
///
/// Six workspaces receive the SAME graph — the Zachary karate club, chosen
/// because it is asymmetric enough to actually break: 20 permuted runs without
/// canonicalisation produced 8 distinct partitions during Task 2. Each
/// workspace creates its entities in a different order, so their uuid orders
/// are six independent random permutations. All six partitions must be
/// identical when compared by name.
#[tokio::test]
async fn node_order_is_canonicalised_by_name_so_random_entity_ids_cannot_change_the_partition() {
    let pool = connect().await;
    let run = Uuid::new_v4().simple().to_string();

    let names: Vec<String> = (0..KARATE_NODES).map(|i| format!("n{i:02}")).collect();
    let edges: Vec<(&str, &str, f32)> = KARATE_EDGES
        .iter()
        .map(|&(a, b)| (names[a].as_str(), names[b].as_str(), 1.0f32))
        .collect();

    let mut partitions = Vec::new();
    for attempt in 0..6 {
        let ws = workspace(&pool).await;
        let pg = page(&pool, ws).await;

        // A different creation order each time. Rotating by a stride coprime
        // with 34 gives six genuinely different orders without an RNG, so this
        // test is itself deterministic.
        let stride = 1 + 2 * attempt;
        let order: Vec<&str> = (0..KARATE_NODES)
            .map(|i| names[(i * stride + attempt) % KARATE_NODES].as_str())
            .collect();
        let mut order: Vec<&str> = {
            let mut seen = BTreeSet::new();
            let mut o: Vec<&str> = order.into_iter().filter(|n| seen.insert(*n)).collect();
            for n in &names {
                if seen.insert(n.as_str()) {
                    o.push(n.as_str());
                }
            }
            o
        };
        order.dedup();
        assert_eq!(
            order.len(),
            KARATE_NODES,
            "sanity: every karate node must be created exactly once"
        );

        seed_graph(&pool, ws, pg, &run, &format!("k{attempt}"), &order, &edges).await;
        CommunityWorker::new(pool.clone(), ws)
            .cold_run()
            .await
            .unwrap();
        partitions.push(stored_partition(&pool, ws).await);
    }

    assert_eq!(
        partitions[0].len(),
        4,
        "sanity: the karate club clusters into 4 communities — an empty or singleton partition \
         would make the equality below vacuous"
    );
    for (i, p) in partitions.iter().enumerate().skip(1) {
        assert_eq!(
            *p, partitions[0],
            "workspace {i} built the same graph in a different entity-creation order and got a \
             different partition; node ordering is leaking entity ids"
        );
    }
}

/// Task 1 deliberately shipped `bump_churn`/`churn` with nothing that ever
/// resets them — that is this task's job, and without it the cold path fires on
/// every subsequent edit forever, because churn only ever grows.
///
/// The timestamp assertion is strictly `>`, against a value read from the
/// DATABASE's clock before the run. A `>=` would pass against a `last_full_run_at`
/// that was never written at all if the column defaulted to that instant, and
/// this project has already shipped one timestamp test that could not fail.
#[tokio::test]
async fn a_cold_run_resets_churn_and_stamps_last_full_run_at() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();
    seed_graph(&pool, ws, pg, &run, "g", TWO_CLIQUE_NODES, TWO_CLIQUES).await;

    community::bump_churn(&pool, ws, 7).await.unwrap();
    let (before_count, before_stamp) = community::churn(&pool, ws).await.unwrap();
    assert_eq!(
        (before_count, before_stamp.is_some()),
        (7, false),
        "sanity: churn accumulated and no full run has happened yet"
    );

    // The instant is read from, and compared in, the DATABASE's clock — never
    // the test process's — so no client skew can make the comparison lie.
    // `noted-index` does not depend on `chrono`, hence the text round trip.
    let t0: String = sqlx::query_scalar("SELECT now()::text")
        .fetch_one(&pool)
        .await
        .unwrap();

    CommunityWorker::new(pool.clone(), ws)
        .cold_run()
        .await
        .unwrap();

    let (after_count, after_stamp) = community::churn(&pool, ws).await.unwrap();
    assert_eq!(
        after_count, 0,
        "a completed cold run must zero the churn counter, or the cold path fires forever after"
    );
    assert!(
        after_stamp.is_some(),
        "a completed cold run must stamp last_full_run_at"
    );
    let strictly_later: bool = sqlx::query_scalar(
        "SELECT last_full_run_at > $2::timestamptz FROM graph_churn WHERE workspace_id = $1",
    )
    .bind(ws)
    .bind(&t0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        strictly_later,
        "last_full_run_at must be STRICTLY later than {t0}, the instant before the run"
    );
}

/// The threshold is what separates the hot path from the cold one. A workspace
/// under it must be served by the cheap approximation ONLY.
///
/// 41 clusterable edges, and the 41 is chosen rather than incidental: it is the
/// smallest fixture on which `ceil` and `floor` DISAGREE.
/// `0.05 * 41 = 2.05`, so the threshold is 3 under the `ceil` the code specifies
/// and 2 under a `floor`. The earlier 40-edge version put the boundary at
/// `0.05 * 40 = 2.0` exactly, where the two rounding modes coincide — so
/// replacing `ceil` with `floor` in `cold_run_if_due` left this test, and the
/// whole suite, green. A test of a rounding rule has to sit somewhere the
/// rounding actually rounds.
///
/// (An earlier revision of this comment justified the fixture size by "the
/// `max(1, ...)` floor". That mechanism was found unreachable and DELETED —
/// `ceil` already returns at least 1 for any non-empty graph, and the empty
/// graph returns earlier at the `changed <= 0` guard. See the note in
/// `CommunityWorker::cold_run_if_due`.)
#[tokio::test]
async fn the_cold_path_fires_only_once_churn_crosses_the_threshold() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();

    let names: Vec<String> = (0..42).map(|i| format!("c{i:02}")).collect();
    let order: Vec<&str> = names.iter().map(String::as_str).collect();
    let edges: Vec<(&str, &str, f32)> = (0..41)
        .map(|i| (names[i].as_str(), names[i + 1].as_str(), 1.0f32))
        .collect();
    seed_graph(&pool, ws, pg, &run, "chain", &order, &edges).await;

    assert_eq!(
        community::clusterable_edge_count(&pool, ws).await.unwrap(),
        41,
        "sanity: the threshold is a fraction of THIS number, and 41 is what separates \
         ceil(2.05) = 3 from floor(2.05) = 2"
    );

    let worker = CommunityWorker::new(pool.clone(), ws);

    let first = worker.on_edges_changed(&[], 1).await.unwrap();
    assert!(
        !first.cold_run,
        "1 changed edge out of 41 is under the 5% threshold and must NOT trigger a full re-cluster"
    );
    assert!(
        stored_partition(&pool, ws).await.is_empty(),
        "no cold run has happened, so there is no partition yet — proving the assertion above is \
         about the cold path and not merely about a return value"
    );

    // THE ROUNDING STEP. Churn reaches 2, which is `floor(0.05 * 41)` but not
    // `ceil(0.05 * 41)`. A cold run here means the threshold rounded down.
    let at_floor = worker.on_edges_changed(&[], 1).await.unwrap();
    assert!(
        !at_floor.cold_run,
        "2 changed edges reach floor(0.05 * 41) but not ceil(0.05 * 41) = 3; rounding DOWN would \
         fire the cold path here, and the threshold is specified as a ceiling"
    );
    assert!(
        stored_partition(&pool, ws).await.is_empty(),
        "and no partition may have been written behind that return value either"
    );

    let second = worker.on_edges_changed(&[], 1).await.unwrap();
    assert!(
        second.cold_run,
        "the third changed edge takes churn to 3, which reaches ceil(0.05 * 41); the cold path \
         must fire"
    );
    assert!(
        !stored_partition(&pool, ws).await.is_empty(),
        "the cold run must actually have clustered something"
    );
    assert_eq!(
        community::churn(&pool, ws).await.unwrap().0,
        0,
        "and it must have reset the counter it consumed"
    );
}

/// The hot path's one decision: the community of the MOST STRONGLY CONNECTED
/// neighbour.
///
/// `new` is attached to both clusters — weight 1 to the `aa` clique and weight
/// 5 to the `xa` clique. Weight is the only thing distinguishing them, so a hot
/// path that picked an arbitrary neighbour, or the weakest, or the first row
/// Postgres happened to return, lands in the wrong community.
#[tokio::test]
async fn the_hot_path_joins_the_community_of_the_most_strongly_connected_neighbour() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();

    seed_graph(&pool, ws, pg, &run, "g", TWO_CLIQUE_NODES, TWO_CLIQUES).await;
    let worker = CommunityWorker::new(pool.clone(), ws);
    worker.cold_run().await.unwrap();
    assert_eq!(
        stored_partition(&pool, ws).await,
        two_clique_partition(),
        "sanity: the two cliques must be clustered before the hot path is asked anything"
    );

    // A weak link to the `a` clique and a strong one to the `x` clique.
    let new_edges: &[(&str, &str, f32)] = &[("new", "aa", 1.0), ("new", "xd", 5.0)];
    let ids = seed_graph(&pool, ws, pg, &run, "new", &["new"], new_edges).await;

    let moved = worker.hot_reassign(&[ids["new"]]).await.unwrap();
    assert_eq!(
        moved, 1,
        "the new entity has clustered neighbours and must move"
    );

    let home = community_of(&pool, ws, "new")
        .await
        .expect("the hot path must have given the new entity a community");
    assert!(
        home.contains(&"xd".to_string()),
        "the new entity's strongest link (weight 5) is to the x clique, so that is where it \
         belongs; it landed in {home:?}"
    );
    assert!(
        !home.contains(&"aa".to_string()),
        "and it must NOT have followed its weight-1 link into the a clique; got {home:?}"
    );
}

/// The hot path's REASON TO EXIST: the partition stays queryable between cold
/// runs. An edit under the churn threshold must still leave every newly-linked
/// entity in a community, with no clustering having run.
///
/// This is the assertion that makes the hot path load-bearing rather than
/// decorative. Without it the whole module passes its tests with
/// `hot_reassign` deleted — measured, not supposed: mutating
/// `on_edges_changed` to skip the hot path entirely killed no test until this
/// one existed. The convergence property cannot catch it either, and for a
/// structural reason worth naming: the cold path recomputes the partition from
/// `edges` alone, so it arrives at the same answer whether or not the hot path
/// ever ran.
#[tokio::test]
async fn an_edit_under_the_threshold_is_served_by_the_hot_path_with_no_cold_run() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();

    // 40 edges, so the threshold is 2 and a single changed edge stays under it.
    let names: Vec<String> = (0..41).map(|i| format!("c{i:02}")).collect();
    let order: Vec<&str> = names.iter().map(String::as_str).collect();
    let edges: Vec<(&str, &str, f32)> = (0..40)
        .map(|i| (names[i].as_str(), names[i + 1].as_str(), 1.0f32))
        .collect();
    seed_graph(&pool, ws, pg, &run, "chain", &order, &edges).await;

    let worker = CommunityWorker::new(pool.clone(), ws);
    worker.cold_run().await.unwrap();
    let baseline = stored_partition(&pool, ws).await;
    assert!(
        !baseline.is_empty(),
        "sanity: there is a partition to extend"
    );
    assert_eq!(
        community_of(&pool, ws, "fresh").await,
        None,
        "sanity: the entity about to be added does not exist yet"
    );

    let new_edge: &[(&str, &str, f32)] = &[("fresh", "c20", 1.0)];
    let ids = seed_graph(&pool, ws, pg, &run, "fresh", &["fresh"], new_edge).await;

    let outcome = worker.on_edges_changed(&[ids["fresh"]], 1).await.unwrap();

    assert!(
        !outcome.cold_run,
        "one changed edge out of 41 is under the threshold; no clustering may have run"
    );
    assert_eq!(
        outcome.reassigned, 1,
        "the hot path must have placed the new entity — this is the number that proves \
         on_edges_changed actually invokes it"
    );
    let home = community_of(&pool, ws, "fresh")
        .await
        .expect("the new entity must be queryable in a community immediately, with no cold run");
    assert!(
        home.contains(&"c20".to_string()),
        "and it must be in its only neighbour's community; got {home:?}"
    );
}

/// The hot path's tie-break. Two neighbours at EXACTLY equal weight in two
/// different communities are a genuine tie, and without a deterministic second
/// key the answer is whichever row Postgres happened to return first — the hot
/// path would be irreproducible, and the resulting flakiness would surface much
/// later as an unexplained partition difference.
///
/// `tied` links to `aa` and to `xd` at weight 2.0 each, from bit-identical
/// operands. The documented rule is neighbour name ascending, and `aa` < `xd`.
///
/// The mutation this test is written against is `ne.name ASC` -> `DESC`, which
/// lands `tied` in the x clique. Note honestly that DELETING the tie-break
/// clause outright is NOT reliably detectable by any test: Postgres is then
/// free to return either row and will usually return the same one every time,
/// which is precisely the silent non-determinism the clause exists to prevent.
/// Pinning the direction is the strongest available evidence that the code path
/// is reached at all.
#[tokio::test]
async fn the_hot_paths_tie_break_between_equally_weighted_neighbours_is_deterministic() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();

    seed_graph(&pool, ws, pg, &run, "g", TWO_CLIQUE_NODES, TWO_CLIQUES).await;
    let worker = CommunityWorker::new(pool.clone(), ws);
    worker.cold_run().await.unwrap();
    assert_eq!(
        stored_partition(&pool, ws).await,
        two_clique_partition(),
        "sanity: two communities are needed for a tie between them to mean anything"
    );

    let tied_edges: &[(&str, &str, f32)] = &[("tied", "aa", 2.0), ("tied", "xd", 2.0)];
    let ids = seed_graph(&pool, ws, pg, &run, "tied", &["tied"], tied_edges).await;
    worker.hot_reassign(&[ids["tied"]]).await.unwrap();

    let home = community_of(&pool, ws, "tied")
        .await
        .expect("a tied entity must still be given a community, not left out");
    assert!(
        home.contains(&"aa".to_string()),
        "an exact weight tie must resolve by neighbour name ascending ('aa' before 'xd'); it \
         landed in {home:?}"
    );
}

/// The hot path is a WRITE to `communities`, and it must leave that table in a
/// state the next cold-path swap can work with.
///
/// Two invariants, both load-bearing rather than tidy:
///
///   * `member_set_hash` must stay true to the membership. It is what
///     `swap_partition` matches on to preserve a community's row and therefore
///     its summary. A stale hash makes the next cold run fail to recognise a
///     community it has just re-derived unchanged, dropping the row and
///     regenerating a summary — an LLM call — for nothing.
///   * A community emptied by a move must be DELETED, not left as a zero-member
///     row. A memberless community describes nothing, cannot be summarised, and
///     would eventually collide with a second emptied one on
///     `UNIQUE (workspace_id, level, member_set_hash)` (both hash the empty
///     set), aborting a hot-path write that is meant to be infallible.
#[tokio::test]
async fn the_hot_path_keeps_stored_hashes_true_and_removes_communities_it_empties() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();

    seed_graph(&pool, ws, pg, &run, "g", TWO_CLIQUE_NODES, TWO_CLIQUES).await;
    let worker = CommunityWorker::new(pool.clone(), ws);
    worker.cold_run().await.unwrap();
    assert_eq!(
        stored_partition(&pool, ws).await.len(),
        2,
        "sanity: one of these two communities is about to be emptied"
    );

    // Every member of the `a` clique gains an overwhelming link into the `x`
    // clique, so the hot path drains the `a` community completely.
    let pull: &[(&str, &str, f32)] = &[
        ("aa", "xa", 10.0),
        ("ab", "xa", 10.0),
        ("ac", "xa", 10.0),
        ("ad", "xa", 10.0),
    ];
    let ids = seed_graph(&pool, ws, pg, &run, "pull", &[], pull).await;
    let movers: Vec<Uuid> = ["aa", "ab", "ac", "ad"].iter().map(|n| ids[*n]).collect();
    worker.hot_reassign(&movers).await.unwrap();

    let rows: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, member_set_hash FROM communities WHERE workspace_id = $1")
            .bind(ws)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the emptied community must be deleted, not left behind with zero members"
    );

    for (id, stored_hash) in rows {
        let members: Vec<Uuid> =
            sqlx::query_scalar("SELECT entity_id FROM community_members WHERE community_id = $1")
                .bind(id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(members.len(), 8, "all eight entities ended up together");
        assert_eq!(
            stored_hash,
            community::member_set_hash(&members),
            "the stored member_set_hash must describe the membership the hot path actually left \
             behind, or the next cold run regenerates summaries for communities that did not move"
        );
    }
}

/// Tenancy. Clustering workspace A must not read, move, or renumber anything in
/// workspace B — not merely "not corrupt it", but leave B's rows byte-identical,
/// because B's summaries will be keyed by B's community ids.
///
/// Both halves are asserted, because they fail to different mutations: A's
/// partition must not contain B's entities (a missing filter on the READ), and
/// B's community rows must be untouched (a missing filter on the WRITE).
#[tokio::test]
async fn clustering_one_workspace_cannot_touch_another() {
    let pool = connect().await;
    let ws_a = workspace(&pool).await;
    let ws_b = workspace(&pool).await;
    let page_a = page(&pool, ws_a).await;
    let page_b = page(&pool, ws_b).await;
    let run = Uuid::new_v4().simple().to_string();

    seed_graph(
        &pool,
        ws_a,
        page_a,
        &run,
        "a",
        TWO_CLIQUE_NODES,
        TWO_CLIQUES,
    )
    .await;
    // B's graph uses DIFFERENT names, so an entity leaking across the boundary
    // is visible by name rather than merely by count.
    let b_edges: &[(&str, &str, f32)] = &[
        ("bb1", "bb2", 1.0),
        ("bb1", "bb3", 1.0),
        ("bb2", "bb3", 1.0),
    ];
    seed_graph(
        &pool,
        ws_b,
        page_b,
        &run,
        "b",
        &["bb1", "bb2", "bb3"],
        b_edges,
    )
    .await;

    CommunityWorker::new(pool.clone(), ws_b)
        .cold_run()
        .await
        .unwrap();
    let b_rows_before: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT c.id, c.member_set_hash, e.name
         FROM communities c
         JOIN community_members cm ON cm.community_id = c.id
         JOIN entities e ON e.id = cm.entity_id
         WHERE c.workspace_id = $1
         ORDER BY c.id, e.name",
    )
    .bind(ws_b)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        b_rows_before.len(),
        3,
        "sanity: B must have a real partition to be damaged"
    );

    let worker_a = CommunityWorker::new(pool.clone(), ws_a);
    worker_a.cold_run().await.unwrap();
    assert_eq!(
        stored_partition(&pool, ws_a).await,
        two_clique_partition(),
        "A's cold run must cluster A's entities and ONLY A's — B's graph is live in the same \
         database and must be invisible to it"
    );

    let a_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM entities WHERE workspace_id = $1")
        .bind(ws_a)
        .fetch_all(&pool)
        .await
        .unwrap();
    worker_a.hot_reassign(&a_ids).await.unwrap();

    // NOT asserted: that A's partition still equals the cliques. Hot-reassigning
    // an ENTIRE graph legitimately collapses it — each entity chases its
    // strongest neighbour's community and the moves cascade. That is the
    // approximation behaving as designed on an input the production path never
    // sends it (only entities touched by an edit), and the cold run corrects it.
    // What must hold regardless is that nothing of B's ever appears.
    let a_names: BTreeSet<String> = stored_partition(&pool, ws_a)
        .await
        .into_iter()
        .flatten()
        .collect();
    assert!(
        !a_names.is_empty() && a_names.iter().all(|n| !n.starts_with("bb")),
        "no entity of B's may appear in A's partition, before or after the hot path; got {a_names:?}"
    );

    let b_rows_after: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT c.id, c.member_set_hash, e.name
         FROM communities c
         JOIN community_members cm ON cm.community_id = c.id
         JOIN entities e ON e.id = cm.entity_id
         WHERE c.workspace_id = $1
         ORDER BY c.id, e.name",
    )
    .bind(ws_b)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        b_rows_after, b_rows_before,
        "B's community rows — ids, hashes and membership — must be byte-identical after A \
         clustered and hot-reassigned its entire graph"
    );
}

// ------------------------------------------------------------ crown jewel --

/// A `StubExtractor` under a per-test `model_id`, so one test's
/// `chunk_extractions` markers can never satisfy another's queue on the shared
/// dev database.
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

/// Put `pairs` on `pg` as one chunk per edge and extract them through the REAL
/// extraction worker. Replaces the page's chunk list wholesale, so a pair
/// dropped from `pairs` is a genuine edit that kills its edges.
async fn rewrite_page_and_extract(
    pool: &noted_db::PgPool,
    ws: Uuid,
    pg: Uuid,
    run: &str,
    provider: &Arc<TaggedStub>,
    pairs: &[(&str, &str)],
) {
    let mut hashes = Vec::new();
    for (a, b) in pairs {
        let text = format!("{a} {b} {run}");
        let hash = format!("cwx-{run}-{a}-{b}");
        noted_db::chunks::upsert(pool, &[(hash.clone(), text, 10)])
            .await
            .unwrap();
        hashes.push(hash);
    }
    noted_db::chunks::set_page_chunks(pool, pg, &hashes)
        .await
        .unwrap();
    ExtractWorker::new_scoped(pool.clone(), provider.clone(), ws)
        .drain()
        .await
        .unwrap();
}

async fn entity_ids(pool: &noted_db::PgPool, ws: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar("SELECT id FROM entities WHERE workspace_id = $1 ORDER BY name")
        .bind(ws)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// THE CROWN JEWEL (spec §5). After any sequence of edits and hot-path
/// reassignments, running the cold path produces the same partition as running
/// Louvain from scratch on the final graph.
///
/// That is what licenses the approximation. If it fails, the hot path is
/// CORRUPTING state rather than approximating it, and the whole hot/cold design
/// is unsound.
///
/// # How it is routed
///
/// Workspace A lives a history: chunks are written and extracted through the
/// real `ExtractWorker`, and every edit is followed by
/// `CommunityWorker::on_edges_changed` — the production entry point, which
/// hot-reassigns, accumulates churn, and decides for itself whether to cold-run.
/// Nothing here calls a helper the product would not call. M2a's equivalent
/// property test called `apply_extraction` directly and skipped `ExtractWorker`,
/// and that milestone's Critical bug lived precisely in the seam it skipped.
///
/// Workspace B is the control: a VIRGIN workspace that only ever sees A's FINAL
/// content, extracted once and cold-run once. No edits, no hot path, no history.
///
/// The comparison is by entity NAME, in canonical form. B's entity ids are
/// entirely different random uuids from A's, so this simultaneously re-proves
/// that node ordering does not leak ids — a rebuild comparison is exactly the
/// scenario the spec warns would fail spuriously under id ordering.
///
/// # WHAT THIS PROVES, AND WHAT IT DOES NOT
///
/// It proves the hot path APPROXIMATES rather than CORRUPTS: no amount of
/// intermediate local reassignment leaves residue that changes the answer the
/// cold path arrives at. It proves the two paths agree about which graph they
/// are looking at, that dead edges from removed chunks really do leave the
/// graph, and that a cold run actually fires by the end instead of leaving the
/// hot path's guesses standing.
///
/// It proves NOTHING about a real LLM. Like M2a's `incremental == full rebuild`,
/// this is a property of a DETERMINISTIC pipeline: `StubExtractor` re-reading
/// the same chunk emits the same entities and edges every time. A real model
/// disagrees with itself, so A's history and B's single pass would build
/// genuinely different graphs and this equality could not hold — not because
/// anything here is broken, but because the premise would be false. There is
/// still no LLM in this environment; the real-model run remains a documented
/// operator step.
///
/// Be honest about one more thing: the cold path recomputes the partition from
/// `edges` alone and never reads `communities`, so partition equality is close
/// to structural. What this test genuinely catches is the SEAM — the hot path
/// leaving `communities` in a state the swap cannot overwrite (an emptied
/// community colliding on `UNIQUE (workspace_id, level, member_set_hash)` is a
/// real, reachable version of that), churn accounting that never lets the cold
/// path fire at all, and the two paths disagreeing about liveness. Those are
/// where the bug would actually be, and they are not structural.
#[tokio::test]
async fn a_history_of_edits_and_hot_path_moves_converges_on_a_from_scratch_rebuild() {
    let pool = connect().await;
    let run = Uuid::new_v4().simple().to_string();
    let provider = Arc::new(TaggedStub(format!("communities-crown-{run}")));

    let ws_a = workspace(&pool).await;
    let page_a = page(&pool, ws_a).await;
    let worker_a = CommunityWorker::new(pool.clone(), ws_a);

    // ---- history ---------------------------------------------------------
    // Two dense-ish groups plus a bridge, edited four times: grown, rewired,
    // pruned, then grown again. Every step goes through the extraction worker
    // and then through the community worker's production entry point.
    let step1: &[(&str, &str)] = &[
        ("Aaa", "Aab"),
        ("Aab", "Aac"),
        ("Aac", "Aaa"),
        ("Xaa", "Xab"),
        ("Xab", "Xac"),
        ("Xac", "Xaa"),
        ("Aaa", "Xaa"),
    ];
    let step2: &[(&str, &str)] = &[
        ("Aaa", "Aab"),
        ("Aab", "Aac"),
        ("Aac", "Aaa"),
        ("Aad", "Aaa"),
        ("Aad", "Aab"),
        ("Xaa", "Xab"),
        ("Xab", "Xac"),
        ("Xac", "Xaa"),
        ("Xad", "Xaa"),
        ("Xad", "Xab"),
        ("Aaa", "Xaa"),
    ];
    // An edit that REMOVES edges: the Aac triangle is broken and the bridge
    // rewired. Removed pairs' edges become dead the moment their chunks leave
    // the page, which is the case the hot path most needs to survive.
    let step3: &[(&str, &str)] = &[
        ("Aaa", "Aab"),
        ("Aad", "Aaa"),
        ("Aad", "Aab"),
        ("Xaa", "Xab"),
        ("Xab", "Xac"),
        ("Xac", "Xaa"),
        ("Xad", "Xaa"),
        ("Xad", "Xab"),
        ("Aab", "Xac"),
    ];
    let step4: &[(&str, &str)] = &[
        ("Aaa", "Aab"),
        ("Aad", "Aaa"),
        ("Aad", "Aab"),
        ("Aae", "Aad"),
        ("Aae", "Aaa"),
        ("Xaa", "Xab"),
        ("Xab", "Xac"),
        ("Xac", "Xaa"),
        ("Xad", "Xaa"),
        ("Xad", "Xab"),
        ("Aab", "Xac"),
    ];

    let mut cold_runs = 0usize;
    let mut hot_moves = 0usize;
    for (i, step) in [step1, step2, step3, step4].iter().enumerate() {
        rewrite_page_and_extract(&pool, ws_a, page_a, &run, &provider, step).await;
        let affected = entity_ids(&pool, ws_a).await;
        // `step.len()` edges were rewritten; that is the churn this edit owes.
        let outcome = worker_a
            .on_edges_changed(&affected, step.len() as i64)
            .await
            .unwrap();
        if outcome.cold_run {
            cold_runs += 1;
        }
        hot_moves += outcome.reassigned;
        assert!(
            !stored_partition(&pool, ws_a).await.is_empty(),
            "the partition must be available at every point in the history (step {i}) — that is \
             the entire reason the hot path exists"
        );
    }
    assert!(
        cold_runs > 0,
        "sanity: the history must actually have exercised the cold path; if the threshold never \
         fired, this test would be comparing two cold runs and proving nothing about the hot one"
    );
    assert!(
        hot_moves > 0,
        "sanity: the history must actually have exercised the HOT path too. Without this the \
         test converges just as happily with hot_reassign deleted, because the cold path \
         recomputes from `edges` and never reads `communities`"
    );

    // A final edit-free settle through the SAME production entry point, so the
    // last hot-path moves are definitely absorbed by a cold run.
    let affected = entity_ids(&pool, ws_a).await;
    let settle = worker_a
        .on_edges_changed(&affected, i64::from(u32::MAX))
        .await
        .unwrap();
    assert!(
        settle.cold_run,
        "the settling call must trigger the cold path"
    );

    // ---- the control: a virgin workspace holding only the final content ----
    let ws_b = workspace(&pool).await;
    let page_b = page(&pool, ws_b).await;
    rewrite_page_and_extract(&pool, ws_b, page_b, &run, &provider, step4).await;
    CommunityWorker::new(pool.clone(), ws_b)
        .cold_run()
        .await
        .unwrap();

    let a = stored_partition(&pool, ws_a).await;
    let b = stored_partition(&pool, ws_b).await;
    assert!(
        b.len() > 1,
        "sanity: the reference partition must have real structure ({b:?}); comparing two \
         single-community (or empty) partitions is a test that cannot fail"
    );
    assert_eq!(
        a, b,
        "a workspace that lived through four edits and every hot-path reassignment they caused \
         must end up with EXACTLY the partition a from-scratch rebuild of its final graph \
         produces"
    );

    // The reference is also what Louvain says directly about A's own final
    // graph — closing the loop that the two workspaces really do hold the same
    // graph, rather than agreeing because both are equally wrong.
    let (nodes, edges) = community::clusterable_graph(&pool, ws_a).await.unwrap();
    let index: HashMap<Uuid, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (*id, i))
        .collect();
    let indexed: Vec<(usize, usize, f64)> = edges
        .iter()
        .map(|(x, y, w)| (index[x], index[y], *w))
        .collect();
    let direct: BTreeSet<Vec<String>> = louvain(nodes.len(), &indexed)
        .communities()
        .iter()
        .map(|c| {
            let mut names: Vec<String> = c.iter().map(|&i| nodes[i].1.clone()).collect();
            names.sort();
            names
        })
        .collect();
    assert_eq!(
        a, direct,
        "and it must equal Louvain run directly over A's final clusterable graph"
    );
}

// ------------------------------------------------------------------ karate --

/// Zachary's karate club. Asymmetric enough that node ordering genuinely
/// changes the answer — Task 2 measured 8 distinct partitions across 20
/// permuted runs without canonicalisation — which is exactly what makes it the
/// right graph for an ordering test and the wrong one for a tie-breaking test.
const KARATE_NODES: usize = 34;
const KARATE_EDGES: [(usize, usize); 78] = [
    (0, 1),
    (0, 2),
    (0, 3),
    (0, 4),
    (0, 5),
    (0, 6),
    (0, 7),
    (0, 8),
    (0, 10),
    (0, 11),
    (0, 12),
    (0, 13),
    (0, 17),
    (0, 19),
    (0, 21),
    (0, 31),
    (1, 2),
    (1, 3),
    (1, 7),
    (1, 13),
    (1, 17),
    (1, 19),
    (1, 21),
    (1, 30),
    (2, 3),
    (2, 7),
    (2, 8),
    (2, 9),
    (2, 13),
    (2, 27),
    (2, 28),
    (2, 32),
    (3, 7),
    (3, 12),
    (3, 13),
    (4, 6),
    (4, 10),
    (5, 6),
    (5, 10),
    (5, 16),
    (6, 16),
    (8, 30),
    (8, 32),
    (8, 33),
    (9, 33),
    (13, 33),
    (14, 32),
    (14, 33),
    (15, 32),
    (15, 33),
    (18, 32),
    (18, 33),
    (19, 33),
    (20, 32),
    (20, 33),
    (22, 32),
    (22, 33),
    (23, 25),
    (23, 27),
    (23, 29),
    (23, 32),
    (23, 33),
    (24, 25),
    (24, 27),
    (24, 31),
    (25, 31),
    (26, 29),
    (26, 33),
    (27, 33),
    (28, 31),
    (28, 33),
    (29, 32),
    (29, 33),
    (30, 32),
    (30, 33),
    (31, 32),
    (31, 33),
    (32, 33),
];

/// `mark_full_run` runs AFTER `swap_partition`, never before — and until this
/// test, swapping those two lines survived the entire workspace.
///
/// The ordering is the difference between a failed cold run that still owes a
/// cold run and one that has erased the evidence it is owed. `mark_full_run`
/// zeroes the churn counter and stamps `last_full_run_at` in one statement, and
/// it is `pool`-scoped, so it commits on its own regardless of what the swap
/// does. Run first, it would report a freshly-clustered workspace whose
/// partition is in fact whatever the previous run left; and because the churn
/// counter is the ONLY thing that makes the cold path fire again, the workspace
/// would then have to accumulate a whole fresh threshold's worth of edits before
/// anything retried. Silent and self-perpetuating — the error direction this
/// project has already been bitten by twice.
///
/// A cold run that succeeds proves nothing about ordering, so the failure is
/// FORCED, and it has to land inside `swap_partition` specifically. A
/// `DEFERRABLE INITIALLY DEFERRED` constraint trigger scoped to this
/// workspace's `communities` rows does that precisely: the swap runs to
/// completion and its COMMIT is refused, which is a failure `cold_run` cannot
/// have already passed. The trigger is torn down before the assertions so a
/// failing assertion leaves no DDL in the shared dev database.
#[tokio::test]
async fn a_cold_run_whose_swap_fails_still_owes_a_cold_run() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();

    seed_graph(
        &pool,
        ws,
        pg,
        &run,
        "owed",
        &["x", "y", "z"],
        &[("x", "y", 1.0), ("y", "z", 1.0)],
    )
    .await;

    community::bump_churn(&pool, ws, 7).await.unwrap();

    // SAFETY (AssertSqlSafe): the interpolated values are a `Uuid` rendered by
    // its own `Display`. DDL cannot take bind parameters.
    let trig = format!("noted_test_swap_fails_{}", ws.simple());
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION {trig}() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'rigged swap failure'; END $$"
    )))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE CONSTRAINT TRIGGER {trig} AFTER INSERT ON communities \
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.workspace_id = '{ws}') \
         EXECUTE FUNCTION {trig}()"
    )))
    .execute(&pool)
    .await
    .unwrap();

    let worker = CommunityWorker::new(pool.clone(), ws);
    let outcome = worker.cold_run().await;

    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER {trig} ON communities"
    )))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP FUNCTION {trig}()")))
        .execute(&pool)
        .await
        .unwrap();

    let err = outcome.expect_err("the rigged cold run must fail");
    assert!(
        format!("{err:?}").contains("rigged swap failure"),
        "sanity: the failure must be the swap's COMMIT being refused, not something incidental: \
         {err:?}"
    );

    let (changed, stamp) = community::churn(&pool, ws).await.unwrap();
    assert_eq!(
        changed, 7,
        "a cold run whose swap failed must leave the churn counter intact — it is the only thing \
         that will make the cold path try again"
    );
    assert!(
        stamp.is_none(),
        "and it must not stamp last_full_run_at, which would claim a full run that never landed"
    );
    assert!(
        stored_partition(&pool, ws).await.is_empty(),
        "sanity: no partition was written, so the stamp above would have been a pure lie"
    );
}

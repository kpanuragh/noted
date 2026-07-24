//! The crown-jewel property: incremental extraction over a mutating corpus
//! must converge to the same LIVE graph as a from-scratch full rebuild of the
//! same final state. See `graph_write::apply_extraction`'s doc comment for
//! the entity/edge resolution rules being exercised here.
use noted_db::PgPool;
use noted_index::extract::{
    ExtractedEdge, ExtractedEntity, Extraction, ExtractionProvider, StubExtractor,
};
use noted_index::extract_worker::ExtractWorker;
use noted_index::graph_write::apply_extraction;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

async fn pool() -> PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

async fn workspace(pool: &PgPool, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ($1) RETURNING id")
        .bind(format!("{name}-{}", Uuid::new_v4()))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn page(pool: &PgPool, ws: Uuid, title: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
        .bind(ws)
        .bind(title)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn content_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

/// Make `text` the page's ONLY live chunk: upsert the (content-addressed,
/// globally shared) chunk row, then point `page_chunks` at just this hash.
/// Calling this twice on the same page with different text simulates an
/// edit: the new hash becomes live, the old one's chunk row / extraction /
/// edges are left behind, orphaned (still in the tables, but no live page
/// references them any more via `page_chunks`).
async fn set_single_live_chunk(pool: &PgPool, page_id: Uuid, text: &str) -> String {
    let hash = content_hash(text);
    noted_db::chunks::upsert(pool, &[(hash.clone(), text.to_string(), 10)])
        .await
        .unwrap();
    noted_db::chunks::set_page_chunks(pool, page_id, &[hash.clone()])
        .await
        .unwrap();
    hash
}

/// Drive extraction through the PRODUCTION path: `ExtractWorker::drain` polls
/// `graph::pending_extraction`, fans each pending chunk out over
/// `graph::workspaces_for_chunk`, and writes + marks per workspace.
///
/// This deliberately does NOT call `apply_extraction` directly. The crown-jewel
/// property is only worth what it covers, and calling `apply_extraction`
/// by hand skips the poll -> fan-out -> mark seam entirely — which is exactly
/// where the "a workspace that joins a shared chunk after extraction never gets
/// a graph" bug lived (the marker was keyed `(content_hash, model_id)` with no
/// workspace, so the second workspace was never queued). A property that
/// bypasses the queue cannot see a queue bug.
///
/// Cost: `StubExtractor` is pure and instant and the drains are
/// workspace-scoped, so each call is one small `pending_extraction` poll plus
/// the same writes the direct call made. Measured at well under a second for
/// the whole file — the coverage is not paid for in runtime.
async fn extract_via_worker(pool: &PgPool, ws: Uuid) {
    ExtractWorker::new_scoped(pool.clone(), Arc::new(StubExtractor::new()), ws)
        .drain()
        .await
        .unwrap();
}

/// Direct, single-workspace application — used only by the tests below that
/// are about `replace_chunk_edges`/`resolve_entity` semantics rather than
/// about the queue. Deliberately NOT used by the crown-jewel property (see
/// `extract_via_worker`): the worker's fan-out would write BOTH workspaces'
/// graphs from one drain, which is correct behaviour but would make
/// "workspace A's edges survive workspace B extracting later" vacuous.
async fn extract_and_apply(pool: &PgPool, ws: Uuid, hash: &str, text: &str) {
    let stub = StubExtractor::new();
    let extraction = stub.extract(text).await.unwrap();
    apply_extraction(pool, ws, hash, stub.model_id(), &extraction)
        .await
        .unwrap();
}

/// The graph over LIVE chunks only, scoped to `workspace_id`.
///
/// Two independent scoping clauses, both necessary:
///  - `p.workspace_id = $1` via `page_chunks -> pages`: the edge's source
///    chunk must be referenced by a live (non-archived-by-omission) page in
///    this workspace. An orphaned chunk (edited away) has no `page_chunks`
///    row pointing at it any more, so its edges are excluded even though the
///    `edges` rows themselves still physically exist.
///  - `se.workspace_id = $1 AND te.workspace_id = $1` via `entities`: both
///    endpoint entities must belong to this workspace. Chunk hashes are
///    GLOBAL / content-addressed (no workspace column on `chunks`), so two
///    workspaces can share a chunk row when their text is byte-identical —
///    this clause is what keeps a shared chunk's edges attributed to the
///    right tenant. It is belt-and-braces rather than the sole defence:
///    `replace_chunk_edges`'s DELETE has itself been workspace-scoped since
///    the `edges.workspace_id` denormalisation (migration
///    `0007_edges_workspace.sql`), so cross-tenant clobbering is prevented at
///    the write, not merely filtered out at the read.
async fn live_graph(pool: &PgPool, workspace_id: Uuid) -> HashSet<(String, String, String)> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT DISTINCT se.name, te.name, e.relation
         FROM edges e
         JOIN entities se ON se.id = e.source_entity
         JOIN entities te ON te.id = e.target_entity
         JOIN page_chunks pc ON pc.content_hash = e.source_chunk_hash
         JOIN pages p ON p.id = pc.page_id
         WHERE p.workspace_id = $1 AND se.workspace_id = $1 AND te.workspace_id = $1",
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter().collect()
}

#[tokio::test]
async fn incremental_extraction_equals_a_full_rebuild() {
    let pool = pool().await;

    // ---- INCREMENTAL workspace: build up 3 pages, then edit one ----
    let ws_inc = workspace(&pool, "incremental").await;
    let p1 = page(&pool, ws_inc, "p1").await;
    let p2 = page(&pool, ws_inc, "p2").await;
    let p3 = page(&pool, ws_inc, "p3").await;

    // Unique-per-run suffixes keep this test immune to pollution from other
    // test files sharing the same live database.
    let run = Uuid::new_v4();

    let p1_text_v1 = format!("Alice Bob Carol {run}");
    let h1_v1 = set_single_live_chunk(&pool, p1, &p1_text_v1).await;
    extract_via_worker(&pool, ws_inc).await;

    let p2_text = format!("Dave Eve {run}");
    let h2 = set_single_live_chunk(&pool, p2, &p2_text).await;
    extract_via_worker(&pool, ws_inc).await;

    let p3_text = format!("Frank Grace Heidi {run}");
    let h3 = set_single_live_chunk(&pool, p3, &p3_text).await;
    extract_via_worker(&pool, ws_inc).await;

    // Edit page 1: new text -> new content hash (chunk identity is the
    // hash). The old hash's chunk row / extraction / edges are left behind,
    // orphaned: page_chunks no longer points page 1 at it.
    let p1_text_v2 = format!("Alice Bob Zara {run}");
    let h1_v2 = set_single_live_chunk(&pool, p1, &p1_text_v2).await;
    assert_ne!(
        h1_v1, h1_v2,
        "editing a chunk's text must produce a new content hash"
    );
    extract_via_worker(&pool, ws_inc).await;

    // ---- FULL workspace: only the FINAL state, extracted once each ----
    let ws_full = workspace(&pool, "full").await;
    let fp1 = page(&pool, ws_full, "p1").await;
    let fp2 = page(&pool, ws_full, "p2").await;
    let fp3 = page(&pool, ws_full, "p3").await;

    let fh1 = set_single_live_chunk(&pool, fp1, &p1_text_v2).await;
    extract_via_worker(&pool, ws_full).await;
    let fh2 = set_single_live_chunk(&pool, fp2, &p2_text).await;
    extract_via_worker(&pool, ws_full).await;
    let fh3 = set_single_live_chunk(&pool, fp3, &p3_text).await;
    extract_via_worker(&pool, ws_full).await;

    assert_eq!(fh1, h1_v2, "same final text must hash identically");
    assert_eq!(fh2, h2, "same final text must hash identically");
    assert_eq!(fh3, h3, "same final text must hash identically");

    // ---- THE ASSERTION ----
    let inc_graph = live_graph(&pool, ws_inc).await;
    let full_graph = live_graph(&pool, ws_full).await;

    assert!(
        !full_graph.is_empty(),
        "sanity: the full rebuild must produce a non-empty graph"
    );
    assert_eq!(
        inc_graph, full_graph,
        "the incrementally-built graph over LIVE chunks must equal a from-scratch full rebuild \
         of the same final state"
    );
}

#[tokio::test]
async fn re_extracting_a_chunk_replaces_exactly_its_edges() {
    let pool = pool().await;
    let ws = workspace(&pool, "reextract").await;
    let page_a = page(&pool, ws, "a").await;
    let page_b = page(&pool, ws, "b").await;
    let run = Uuid::new_v4();

    let text_a = format!("Ann Bo {run}");
    let hash_a = set_single_live_chunk(&pool, page_a, &text_a).await;
    let text_b = format!("Cy Do {run}");
    let hash_b = set_single_live_chunk(&pool, page_b, &text_b).await;

    extract_and_apply(&pool, ws, &hash_a, &text_a).await;
    extract_and_apply(&pool, ws, &hash_b, &text_b).await;

    let model = StubExtractor::new().model_id().to_string();

    let edges_b_before: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM edges WHERE source_chunk_hash = $1 AND model_id = $2",
    )
    .bind(&hash_b)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(edges_b_before > 0, "sanity: chunk B must have edges");

    // Force a re-apply of chunk A's hash with a hand-built, DIFFERENT
    // extraction (the hash is fixed by the text, so we can't get "different
    // output" by re-extracting the same text through the stub -- we build
    // the alternate `Extraction` directly, simulating a model upgrade that
    // re-reads the same chunk and disagrees with itself).
    let alt = Extraction {
        entities: vec![
            ExtractedEntity {
                name: "Zed".into(),
                entity_type: "CONCEPT".into(),
                description: None,
            },
            ExtractedEntity {
                name: "Yak".into(),
                entity_type: "CONCEPT".into(),
                description: None,
            },
        ],
        edges: vec![ExtractedEdge {
            source: "Zed".into(),
            target: "Yak".into(),
            relation: "mentions_with".into(),
            weight: 1.0,
        }],
    };
    apply_extraction(&pool, ws, &hash_a, &model, &alt)
        .await
        .unwrap();

    let edges_a_after: Vec<(String, String)> = sqlx::query_as(
        "SELECT se.name, te.name FROM edges e
         JOIN entities se ON se.id = e.source_entity
         JOIN entities te ON te.id = e.target_entity
         WHERE e.source_chunk_hash = $1 AND e.model_id = $2",
    )
    .bind(&hash_a)
    .bind(&model)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        edges_a_after,
        vec![("zed".to_string(), "yak".to_string())],
        "chunk A's edges must be exactly the re-applied extraction's edges"
    );

    let edges_b_after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM edges WHERE source_chunk_hash = $1 AND model_id = $2",
    )
    .bind(&hash_b)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        edges_b_before, edges_b_after,
        "chunk B's edges must be untouched by chunk A's re-extraction"
    );
}

/// Pins the bug the crown-jewel test above caught: chunk `content_hash` is
/// GLOBAL (shared across workspaces for byte-identical text), but edges
/// belong to a single workspace. Workspace A extracts a chunk; workspace B
/// independently extracts a chunk with the SAME text (same hash). Before the
/// workspace-scoped DELETE fix, B's `apply_extraction` -> `replace_chunk_edges`
/// deleted ALL edges for that `(source_chunk_hash, model_id)` regardless of
/// which workspace they belonged to, so A's live-graph edge count would drop
/// to zero after B extracts. This test asserts A's edges survive B's
/// extraction untouched.
#[tokio::test]
async fn two_workspaces_with_identical_text_keep_separate_graphs() {
    let pool = pool().await;
    let ws_a = workspace(&pool, "shared-a").await;
    let ws_b = workspace(&pool, "shared-b").await;
    let page_a = page(&pool, ws_a, "a").await;
    let page_b = page(&pool, ws_b, "b").await;
    let run = Uuid::new_v4();

    let text = format!("Nina Otto Piper {run}");
    let hash_a = set_single_live_chunk(&pool, page_a, &text).await;
    let hash_b = set_single_live_chunk(&pool, page_b, &text).await;
    assert_eq!(
        hash_a, hash_b,
        "identical text must share the same global content hash"
    );

    extract_and_apply(&pool, ws_a, &hash_a, &text).await;

    let a_graph_before = live_graph(&pool, ws_a).await;
    assert!(
        !a_graph_before.is_empty(),
        "sanity: workspace A must have edges after its own extraction"
    );

    // Workspace B independently extracts the SAME chunk hash.
    extract_and_apply(&pool, ws_b, &hash_b, &text).await;

    let a_graph_after = live_graph(&pool, ws_a).await;
    assert_eq!(
        a_graph_before, a_graph_after,
        "workspace A's edges must be unaffected by workspace B extracting the same shared chunk"
    );

    let b_graph_after = live_graph(&pool, ws_b).await;
    assert!(
        !b_graph_after.is_empty(),
        "sanity: workspace B must also have its own edges for the shared chunk"
    );
}

#[tokio::test]
async fn entities_do_not_leak_across_workspaces() {
    let pool = pool().await;
    let ws1 = workspace(&pool, "leak1").await;
    let ws2 = workspace(&pool, "leak2").await;
    let p1 = page(&pool, ws1, "p").await;
    let p2 = page(&pool, ws2, "p").await;
    let run = Uuid::new_v4();

    let text = format!("Ivy Jack {run}");
    let h1 = set_single_live_chunk(&pool, p1, &text).await;
    let h2 = set_single_live_chunk(&pool, p2, &text).await;
    assert_eq!(
        h1, h2,
        "identical text must share the same global content hash"
    );

    extract_and_apply(&pool, ws1, &h1, &text).await;
    extract_and_apply(&pool, ws2, &h2, &text).await;

    let ids1: HashSet<Uuid> = sqlx::query_scalar("SELECT id FROM entities WHERE workspace_id = $1")
        .bind(ws1)
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .collect();
    let ids2: HashSet<Uuid> = sqlx::query_scalar("SELECT id FROM entities WHERE workspace_id = $1")
        .bind(ws2)
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .collect();

    assert!(
        !ids1.is_empty(),
        "sanity: workspace 1 must have resolved entities"
    );
    assert!(
        !ids2.is_empty(),
        "sanity: workspace 2 must have resolved entities"
    );
    assert!(
        ids1.is_disjoint(&ids2),
        "entity ids must never be shared across workspaces, even for identical text"
    );
}

/// STUB-COVERAGE GAP (a): `StubExtractor` only ever emits SINGLE-TOKEN names,
/// so multi-word entity normalisation — the whitespace-collapsing half of
/// `normalise_entity` — is structurally unreachable through any stub-driven
/// test. This drives it with a hand-built `Extraction` instead: no model
/// needed, the type is just data.
///
/// Two names that differ only in whitespace and case must collapse to ONE
/// entity, and the edge between them must land on the collapsed ids.
#[tokio::test]
async fn multi_word_entity_names_normalise_to_one_entity_through_apply_extraction() {
    let pool = pool().await;
    let ws = workspace(&pool, "multiword").await;
    let p = page(&pool, ws, "p").await;
    let run = Uuid::new_v4();

    let text = format!("multi word {run}");
    let hash = set_single_live_chunk(&pool, p, &text).await;
    let model = format!("multiword-model-{run}");

    let a = format!("Acme   Corp\tHoldings {run}");
    let a_again = format!("acme corp holdings {run}");
    let b = format!("Ada   Lovelace {run}");

    let ex = Extraction {
        entities: vec![
            ExtractedEntity {
                name: a.clone(),
                entity_type: "ORG".into(),
                description: None,
            },
            // The SAME entity, written differently — irregular internal
            // whitespace and different case.
            ExtractedEntity {
                name: a_again.clone(),
                entity_type: "ORG".into(),
                description: None,
            },
            ExtractedEntity {
                name: b.clone(),
                entity_type: "PERSON".into(),
                description: None,
            },
        ],
        edges: vec![ExtractedEdge {
            // Yet another spelling of the same name, on the edge this time.
            source: format!("ACME CORP    HOLDINGS {run}"),
            target: b.clone(),
            relation: "employs".into(),
            weight: 1.0,
        }],
    };
    apply_extraction(&pool, ws, &hash, &model, &ex)
        .await
        .unwrap();

    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM entities WHERE workspace_id = $1 ORDER BY name")
            .bind(ws)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        names,
        vec![
            format!("acme corp holdings {run}"),
            format!("ada lovelace {run}")
        ],
        "three spellings of two names must normalise to exactly two entities, \
         lowercased with runs of whitespace collapsed to single spaces"
    );

    let edges: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT se.name, te.name, e.relation FROM edges e
         JOIN entities se ON se.id = e.source_entity
         JOIN entities te ON te.id = e.target_entity
         WHERE e.workspace_id = $1 AND e.source_chunk_hash = $2 AND e.model_id = $3",
    )
    .bind(ws)
    .bind(&hash)
    .bind(&model)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        edges,
        vec![(
            format!("acme corp holdings {run}"),
            format!("ada lovelace {run}"),
            "employs".to_string()
        )],
        "an edge naming an entity with different whitespace must resolve to the SAME node, \
         not create a duplicate"
    );
}

/// STUB-COVERAGE GAP (b) + the `resolve_entity` description/type decision,
/// exercised through `apply_extraction` rather than the repository directly.
/// The stub always emits `description: None` and `entity_type: "CONCEPT"`, so
/// neither the COALESCE widening nor a reclassification is reachable from it.
#[tokio::test]
async fn a_later_pass_can_enrich_an_entity_it_first_saw_bare() {
    let pool = pool().await;
    let ws = workspace(&pool, "enrich").await;
    let p = page(&pool, ws, "p").await;
    let run = Uuid::new_v4();

    let text = format!("enrich {run}");
    let hash = set_single_live_chunk(&pool, p, &text).await;
    let model = format!("enrich-model-{run}");
    let name = format!("Riemann {run}");

    // Pass 1: the entity appears ONLY as an edge endpoint — apply_extraction
    // resolves it with an unknown type and no description.
    let pass1 = Extraction {
        entities: vec![],
        edges: vec![ExtractedEdge {
            source: name.clone(),
            target: format!("Zeta {run}"),
            relation: "studied".into(),
            weight: 1.0,
        }],
    };
    apply_extraction(&pool, ws, &hash, &model, &pass1)
        .await
        .unwrap();

    let (t, d): (String, Option<String>) = sqlx::query_as(
        "SELECT entity_type, description FROM entities WHERE workspace_id = $1 AND name = $2",
    )
    .bind(ws)
    .bind(format!("riemann {run}"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (t.as_str(), d.as_deref()),
        ("CONCEPT", None),
        "a bare edge endpoint starts as an untyped CONCEPT placeholder"
    );

    // Pass 2: a better-informed extraction names it explicitly.
    let pass2 = Extraction {
        entities: vec![ExtractedEntity {
            name: name.clone(),
            entity_type: "PERSON".into(),
            description: Some("a mathematician".into()),
        }],
        edges: vec![ExtractedEdge {
            source: name.clone(),
            target: format!("Zeta {run}"),
            relation: "studied".into(),
            weight: 1.0,
        }],
    };
    apply_extraction(&pool, ws, &hash, &model, &pass2)
        .await
        .unwrap();

    let (t, d): (String, Option<String>) = sqlx::query_as(
        "SELECT entity_type, description FROM entities WHERE workspace_id = $1 AND name = $2",
    )
    .bind(ws)
    .bind(format!("riemann {run}"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (t.as_str(), d.as_deref()),
        ("PERSON", Some("a mathematician")),
        "an explicit later classification must reclassify AND widen the description"
    );

    // Pass 3 mentions it bare again — neither field may regress.
    apply_extraction(&pool, ws, &hash, &model, &pass1)
        .await
        .unwrap();
    let (t, d): (String, Option<String>) = sqlx::query_as(
        "SELECT entity_type, description FROM entities WHERE workspace_id = $1 AND name = $2",
    )
    .bind(ws)
    .bind(format!("riemann {run}"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (t.as_str(), d.as_deref()),
        ("PERSON", Some("a mathematician")),
        "a later bare mention must not undo what a better-informed pass learned"
    );
}

/// STUB-COVERAGE GAP: the stub's `weight` is always 1.0, so nothing else
/// proves a non-default weight survives the round trip at all.
#[tokio::test]
async fn edge_weights_are_stored_as_given_and_replaced_on_re_extraction() {
    let pool = pool().await;
    let ws = workspace(&pool, "weights").await;
    let p = page(&pool, ws, "p").await;
    let run = Uuid::new_v4();

    let text = format!("weights {run}");
    let hash = set_single_live_chunk(&pool, p, &text).await;
    let model = format!("weight-model-{run}");

    let make = |w: f32| Extraction {
        entities: vec![],
        edges: vec![ExtractedEdge {
            source: format!("Src {run}"),
            target: format!("Dst {run}"),
            relation: "rel".into(),
            weight: w,
        }],
    };

    apply_extraction(&pool, ws, &hash, &model, &make(0.25))
        .await
        .unwrap();
    let w: f32 = sqlx::query_scalar(
        "SELECT weight FROM edges WHERE workspace_id = $1 AND source_chunk_hash = $2",
    )
    .bind(ws)
    .bind(&hash)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(w, 0.25, "a non-default weight must survive the write");

    // Re-extraction at a different weight: the DELETE removes the old row and
    // the new weight replaces it.
    apply_extraction(&pool, ws, &hash, &model, &make(0.75))
        .await
        .unwrap();
    let rows: Vec<f32> = sqlx::query_scalar(
        "SELECT weight FROM edges WHERE workspace_id = $1 AND source_chunk_hash = $2",
    )
    .bind(ws)
    .bind(&hash)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![0.75],
        "re-extraction must leave exactly one edge, at the new weight"
    );
}

/// DOCUMENTS A KNOWN GAP, it does not assert a desirable property.
///
/// `edges` cascade away when their source chunk is deleted, but `entities`
/// rows are never deleted by anything. After an edit orphans a chunk, the
/// entities that chunk introduced survive with ZERO live edges. Nothing reaps
/// them.
///
/// This is deliberately NOT fixed in M2a — a reaper is a real design decision
/// (what about an entity a user has annotated? is "no live edges" really
/// death, or just a temporarily-empty node?) and belongs with the clustering
/// work that first cares. It IS a prerequisite for community detection, which
/// clusters over `entities` and would happily cluster these dead nodes into
/// communities describing content no live page contains.
///
/// If this test ever FAILS because the orphan is gone, that is good news: a
/// reaper landed. Delete the test and the M2b-1 note together.
#[tokio::test]
async fn orphan_entities_survive_an_edit_with_zero_live_edges_a_known_m2b_gap() {
    let pool = pool().await;
    let ws = workspace(&pool, "orphan").await;
    let p = page(&pool, ws, "p").await;
    let run = Uuid::new_v4();

    // v1 mentions Carol; v2 does not.
    let v1 = format!("Alice Bob Carol{run}");
    let h1 = set_single_live_chunk(&pool, p, &v1).await;
    extract_via_worker(&pool, ws).await;

    let v2 = format!("Alice Bob Zara{run}");
    let h2 = set_single_live_chunk(&pool, p, &v2).await;
    assert_ne!(h1, h2);
    extract_via_worker(&pool, ws).await;

    let carol = format!("carol{}", run).to_lowercase();
    let live_edges: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM edges e
         JOIN entities en ON en.id = e.source_entity OR en.id = e.target_entity
         JOIN page_chunks pc ON pc.content_hash = e.source_chunk_hash
         WHERE en.workspace_id = $1 AND en.name = $2",
    )
    .bind(ws)
    .bind(&carol)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        live_edges, 0,
        "sanity: after the edit, the orphaned entity must have no edges from any LIVE chunk"
    );

    let still_there: i64 =
        sqlx::query_scalar("SELECT count(*) FROM entities WHERE workspace_id = $1 AND name = $2")
            .bind(ws)
            .bind(&carol)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        still_there, 1,
        "CURRENT BEHAVIOUR (not an endorsement): nothing deletes entities, so the orphan \
         persists. M2b's clustering would treat it as a live graph node."
    );
}

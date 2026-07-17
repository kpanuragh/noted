//! The crown-jewel property: incremental extraction over a mutating corpus
//! must converge to the same LIVE graph as a from-scratch full rebuild of the
//! same final state. See `graph_write::apply_extraction`'s doc comment for
//! the entity/edge resolution rules being exercised here.
use noted_db::PgPool;
use noted_index::extract::{
    ExtractedEdge, ExtractedEntity, Extraction, ExtractionProvider, StubExtractor,
};
use noted_index::graph_write::apply_extraction;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use uuid::Uuid;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
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
///    right tenant if that ever matters (see the report for whether it does
///    in practice, given `replace_chunk_edges`'s delete is not itself
///    workspace-scoped).
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
    extract_and_apply(&pool, ws_inc, &h1_v1, &p1_text_v1).await;

    let p2_text = format!("Dave Eve {run}");
    let h2 = set_single_live_chunk(&pool, p2, &p2_text).await;
    extract_and_apply(&pool, ws_inc, &h2, &p2_text).await;

    let p3_text = format!("Frank Grace Heidi {run}");
    let h3 = set_single_live_chunk(&pool, p3, &p3_text).await;
    extract_and_apply(&pool, ws_inc, &h3, &p3_text).await;

    // Edit page 1: new text -> new content hash (chunk identity is the
    // hash). The old hash's chunk row / extraction / edges are left behind,
    // orphaned: page_chunks no longer points page 1 at it.
    let p1_text_v2 = format!("Alice Bob Zara {run}");
    let h1_v2 = set_single_live_chunk(&pool, p1, &p1_text_v2).await;
    assert_ne!(
        h1_v1, h1_v2,
        "editing a chunk's text must produce a new content hash"
    );
    extract_and_apply(&pool, ws_inc, &h1_v2, &p1_text_v2).await;

    // ---- FULL workspace: only the FINAL state, extracted once each ----
    let ws_full = workspace(&pool, "full").await;
    let fp1 = page(&pool, ws_full, "p1").await;
    let fp2 = page(&pool, ws_full, "p2").await;
    let fp3 = page(&pool, ws_full, "p3").await;

    let fh1 = set_single_live_chunk(&pool, fp1, &p1_text_v2).await;
    extract_and_apply(&pool, ws_full, &fh1, &p1_text_v2).await;
    let fh2 = set_single_live_chunk(&pool, fp2, &p2_text).await;
    extract_and_apply(&pool, ws_full, &fh2, &p2_text).await;
    let fh3 = set_single_live_chunk(&pool, fp3, &p3_text).await;
    extract_and_apply(&pool, ws_full, &fh3, &p3_text).await;

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

use noted_db::graph;

async fn setup() -> (noted_db::PgPool, uuid::Uuid, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('graph-test') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    let page: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .unwrap();
    (pool, ws, page)
}

/// Mirrors `noted_index::extract::normalise_entity` — duplicated here rather than
/// pulled in as a dependency, since `noted-db` must not depend on `noted-index`
/// (that would create the cycle `noted-index -> noted-db -> noted-index`).
/// `graph::resolve_entity` itself does NOT normalise; it expects an
/// already-normalised key from the caller. This helper stands in for that caller.
fn normalise(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Add a live chunk to a page: the chunk row plus the page_chunks link.
/// Mirrors `tests/chunks.rs`'s `live_chunk` helper.
async fn live_chunk(pool: &noted_db::PgPool, page: uuid::Uuid, hash: &str, text: &str) {
    noted_db::chunks::upsert(pool, &[(hash.to_string(), text.to_string(), 10)])
        .await
        .unwrap();
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT content_hash FROM page_chunks WHERE page_id = $1 ORDER BY chunk_index",
    )
    .bind(page)
    .fetch_all(pool)
    .await
    .unwrap();
    let mut all = existing;
    all.push(hash.to_string());
    noted_db::chunks::set_page_chunks(pool, page, &all)
        .await
        .unwrap();
}

#[tokio::test]
async fn resolve_entity_is_idempotent_by_normalised_name() {
    let (pool, ws, _page) = setup().await;
    let (_, ws2, _page2) = setup().await;

    let id1 = graph::resolve_entity(&pool, ws, &normalise("Postgres"), Some("CONCEPT"), None)
        .await
        .unwrap();
    let id2 = graph::resolve_entity(&pool, ws, &normalise("  postgres "), Some("CONCEPT"), None)
        .await
        .unwrap();
    assert_eq!(
        id1, id2,
        "the same normalised name in the same workspace must resolve to the same entity"
    );

    let id3 = graph::resolve_entity(&pool, ws2, &normalise("Postgres"), Some("CONCEPT"), None)
        .await
        .unwrap();
    assert_ne!(
        id1, id3,
        "the same name in a different workspace must be a different entity node"
    );
}

#[tokio::test]
async fn replace_chunk_edges_writes_edges_and_marks_extracted_for_that_workspace() {
    let (pool, ws, page) = setup().await;
    let model = format!("model-{}", uuid::Uuid::new_v4());
    let h = format!("hash-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "Alice met Bob").await;

    let alice = graph::resolve_entity(&pool, ws, "alice", Some("PERSON"), None)
        .await
        .unwrap();
    let bob = graph::resolve_entity(&pool, ws, "bob", Some("PERSON"), None)
        .await
        .unwrap();
    let carol = graph::resolve_entity(&pool, ws, "carol", Some("PERSON"), None)
        .await
        .unwrap();

    let pending_before = graph::pending_extraction(&pool, &model, Some(ws), 1_000_000)
        .await
        .unwrap();
    assert!(
        pending_before.iter().any(|(hash, _)| hash == &h),
        "sanity: an unextracted live chunk must start out pending"
    );

    let edges = vec![
        (alice, bob, "met".to_string(), 1.0f32),
        (bob, carol, "knows".to_string(), 0.5f32),
    ];
    graph::replace_chunk_edges(&pool, ws, &h, &model, &edges)
        .await
        .unwrap();

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM edges WHERE source_chunk_hash = $1 AND model_id = $2",
    )
    .bind(&h)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 2, "both edges must be written");

    // The marker is written in the SAME transaction as the edges, scoped to
    // THIS workspace (migration 0008). Edges and marker commit together, so a
    // chunk is never marked-but-graphless for a workspace.
    let extracted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chunk_extractions
         WHERE workspace_id = $1 AND content_hash = $2 AND model_id = $3",
    )
    .bind(ws)
    .bind(&h)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        extracted, 1,
        "replace_chunk_edges must mark the chunk extracted for this workspace"
    );

    let pending_after = graph::pending_extraction(&pool, &model, Some(ws), 1_000_000)
        .await
        .unwrap();
    assert!(
        !pending_after.iter().any(|(hash, _)| hash == &h),
        "once its edges and marker have committed, the chunk must leave the queue"
    );
}

/// A second `replace_chunk_edges` for the same (workspace, chunk, model) — a
/// re-extraction — must rewrite the edges without erroring on the marker it
/// already wrote.
#[tokio::test]
async fn replace_chunk_edges_is_re_runnable_against_its_own_marker() {
    let (pool, ws, page) = setup().await;
    let model = format!("model-{}", uuid::Uuid::new_v4());
    let h = format!("hash-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "idempotent marker text").await;

    let e1 = graph::resolve_entity(&pool, ws, "m1", Some("CONCEPT"), None)
        .await
        .unwrap();
    let e2 = graph::resolve_entity(&pool, ws, "m2", Some("CONCEPT"), None)
        .await
        .unwrap();

    graph::replace_chunk_edges(&pool, ws, &h, &model, &[(e1, e2, "r".into(), 1.0)])
        .await
        .unwrap();
    graph::replace_chunk_edges(&pool, ws, &h, &model, &[(e1, e2, "r".into(), 1.0)])
        .await
        .unwrap();

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chunk_extractions
         WHERE workspace_id = $1 AND content_hash = $2 AND model_id = $3",
    )
    .bind(ws)
    .bind(&h)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1, "re-extraction must not double-insert the marker");
}

#[tokio::test]
async fn workspaces_for_chunk_finds_every_workspace_referencing_it() {
    let (pool, ws1, page1) = setup().await;
    let (_, ws2, page2) = setup().await;
    let h = format!("hash-{}", uuid::Uuid::new_v4());

    // Same content hash referenced by live pages in two different
    // workspaces — simulating byte-identical text shared across tenants.
    noted_db::chunks::upsert(&pool, &[(h.clone(), "shared text".to_string(), 10)])
        .await
        .unwrap();
    noted_db::chunks::set_page_chunks(&pool, page1, &[h.clone()])
        .await
        .unwrap();
    noted_db::chunks::set_page_chunks(&pool, page2, &[h.clone()])
        .await
        .unwrap();

    let mut found = graph::workspaces_for_chunk(&pool, &h).await.unwrap();
    found.sort();
    let mut expected = vec![ws1, ws2];
    expected.sort();
    assert_eq!(
        found, expected,
        "workspaces_for_chunk must return every workspace whose live page references the chunk"
    );
}

#[tokio::test]
async fn replace_chunk_edges_only_touches_its_own_chunk() {
    let (pool, ws, page) = setup().await;
    let model = format!("model-{}", uuid::Uuid::new_v4());
    let h_a = format!("hash-a-{}", uuid::Uuid::new_v4());
    let h_b = format!("hash-b-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h_a, "chunk a text").await;
    live_chunk(&pool, page, &h_b, "chunk b text").await;

    let e1 = graph::resolve_entity(&pool, ws, "e1", Some("CONCEPT"), None)
        .await
        .unwrap();
    let e2 = graph::resolve_entity(&pool, ws, "e2", Some("CONCEPT"), None)
        .await
        .unwrap();

    graph::replace_chunk_edges(
        &pool,
        ws,
        &h_a,
        &model,
        &[(e1, e2, "rel-a".to_string(), 1.0)],
    )
    .await
    .unwrap();
    graph::replace_chunk_edges(
        &pool,
        ws,
        &h_b,
        &model,
        &[(e1, e2, "rel-b".to_string(), 1.0)],
    )
    .await
    .unwrap();

    // Replacing A's edges again must not touch B's.
    graph::replace_chunk_edges(
        &pool,
        ws,
        &h_a,
        &model,
        &[(e2, e1, "rel-a2".to_string(), 1.0)],
    )
    .await
    .unwrap();

    let b_edges: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM edges WHERE source_chunk_hash = $1 AND model_id = $2",
    )
    .bind(&h_b)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        b_edges, 1,
        "chunk B's edges must be untouched by chunk A's replace"
    );

    let a_edges: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM edges WHERE source_chunk_hash = $1 AND model_id = $2",
    )
    .bind(&h_a)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(a_edges, 1, "chunk A must have only its latest edge set");
}

#[tokio::test]
async fn pending_extraction_returns_live_chunks_with_no_extraction() {
    let (pool, ws, page) = setup().await;
    let model_a = format!("model-a-{}", uuid::Uuid::new_v4());
    let model_b = format!("model-b-{}", uuid::Uuid::new_v4());
    let h = format!("hash-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "some extractable text").await;

    let pending = graph::pending_extraction(&pool, &model_a, Some(ws), 1_000_000)
        .await
        .unwrap();
    assert!(
        pending.iter().any(|(hash, _)| hash == &h),
        "a live chunk with no extraction must be pending"
    );

    graph::replace_chunk_edges(&pool, ws, &h, &model_a, &[])
        .await
        .unwrap();

    let after = graph::pending_extraction(&pool, &model_a, Some(ws), 1_000_000)
        .await
        .unwrap();
    assert!(
        !after.iter().any(|(hash, _)| hash == &h),
        "after extraction, the chunk must not be pending for that model"
    );

    let other_model = graph::pending_extraction(&pool, &model_b, Some(ws), 1_000_000)
        .await
        .unwrap();
    assert!(
        other_model.iter().any(|(hash, _)| hash == &h),
        "extraction is per-model: a different model must still see it pending"
    );
}

#[tokio::test]
async fn replace_chunk_edges_is_idempotent_on_a_duplicate_edge() {
    let (pool, ws, page) = setup().await;
    let model = format!("model-{}", uuid::Uuid::new_v4());
    let h = format!("hash-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "dup edge text").await;

    let e1 = graph::resolve_entity(&pool, ws, "dup1", Some("CONCEPT"), None)
        .await
        .unwrap();
    let e2 = graph::resolve_entity(&pool, ws, "dup2", Some("CONCEPT"), None)
        .await
        .unwrap();

    // Same edge tuple twice in one slice, at different weights — must not
    // PK-violate on (source_entity, target_entity, relation, source_chunk_hash, model_id).
    let edges = vec![
        (e1, e2, "rel".to_string(), 1.0f32),
        (e1, e2, "rel".to_string(), 9.0f32),
    ];
    graph::replace_chunk_edges(&pool, ws, &h, &model, &edges)
        .await
        .unwrap();

    let rows: Vec<f32> = sqlx::query_scalar(
        "SELECT weight FROM edges WHERE source_chunk_hash = $1 AND model_id = $2
         AND source_entity = $3 AND target_entity = $4 AND relation = 'rel'",
    )
    .bind(&h)
    .bind(&model)
    .bind(e1)
    .bind(e2)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "a duplicate edge must not produce two rows");
    assert_eq!(rows[0], 9.0, "the last write's weight must win");

    // Calling replace_chunk_edges twice with the same edge must also not crash.
    graph::replace_chunk_edges(&pool, ws, &h, &model, &[(e1, e2, "rel".to_string(), 2.0)])
        .await
        .unwrap();
    graph::replace_chunk_edges(&pool, ws, &h, &model, &[(e1, e2, "rel".to_string(), 2.0)])
        .await
        .unwrap();
}

/// ENTITY-TYPE SEMANTICS (see `graph::resolve_entity`'s doc comment for the
/// written decision this pins).
///
/// An EXPLICIT classification is last-write-wins: a later extraction pass that
/// reclassifies "acme" from CONCEPT to ORG must stick, because the later pass
/// is the better-informed one and the alternative is an entity permanently
/// frozen at whatever the first, context-free mention guessed.
///
/// An ABSENT classification (`None`) must NOT overwrite. `None` is how
/// `apply_extraction` resolves an entity that appeared only as an edge
/// endpoint, with no `ExtractedEntity` describing it — it means "I know this
/// node exists, I do not know what it is". Under naive last-write-wins that
/// caller would silently DOWNGRADE a known PERSON/ORG back to the CONCEPT
/// placeholder every time the entity was mentioned in passing.
#[tokio::test]
async fn resolve_entity_reclassifies_on_an_explicit_type_but_never_on_an_unknown_one() {
    let (pool, ws, _page) = setup().await;
    let name = format!("acme-{}", uuid::Uuid::new_v4());

    let id = graph::resolve_entity(&pool, ws, &name, Some("CONCEPT"), None)
        .await
        .unwrap();

    // A later pass reclassifies it explicitly.
    let id2 = graph::resolve_entity(&pool, ws, &name, Some("ORG"), None)
        .await
        .unwrap();
    assert_eq!(id, id2, "reclassification must not create a second entity");

    let t: String = sqlx::query_scalar("SELECT entity_type FROM entities WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        t, "ORG",
        "an explicit later classification must win; silently dropping it strands the entity \
         at its first guess forever"
    );

    // A bare mention (edge endpoint, no type known) must leave ORG alone.
    graph::resolve_entity(&pool, ws, &name, None, None)
        .await
        .unwrap();
    let t2: String = sqlx::query_scalar("SELECT entity_type FROM entities WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        t2, "ORG",
        "an unknown type must not downgrade a known one back to the CONCEPT placeholder"
    );
}

/// A brand-new entity resolved with no known type still needs a NOT NULL
/// `entity_type`; it defaults to the same CONCEPT placeholder a bare mention
/// has always got.
#[tokio::test]
async fn resolve_entity_defaults_an_unknown_type_to_concept_on_insert() {
    let (pool, ws, _page) = setup().await;
    let name = format!("bare-{}", uuid::Uuid::new_v4());

    let id = graph::resolve_entity(&pool, ws, &name, None, None)
        .await
        .unwrap();
    let t: String = sqlx::query_scalar("SELECT entity_type FROM entities WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(t, "CONCEPT");
}

/// The description-widening path (`COALESCE`): the stub extractor never emits
/// a description, so nothing else in the suite reaches this branch.
#[tokio::test]
async fn resolve_entity_widens_a_description_but_never_blanks_one() {
    let (pool, ws, _page) = setup().await;
    let name = format!("desc-{}", uuid::Uuid::new_v4());

    graph::resolve_entity(&pool, ws, &name, Some("CONCEPT"), None)
        .await
        .unwrap();
    let d: Option<String> = sqlx::query_scalar("SELECT description FROM entities WHERE name = $1")
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(d, None, "sanity: no description was supplied yet");

    // A later pass learns a description -> it lands.
    graph::resolve_entity(&pool, ws, &name, Some("CONCEPT"), Some("a database"))
        .await
        .unwrap();
    let d: Option<String> = sqlx::query_scalar("SELECT description FROM entities WHERE name = $1")
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(d.as_deref(), Some("a database"));

    // A later pass with NO description must not blank it out.
    graph::resolve_entity(&pool, ws, &name, Some("CONCEPT"), None)
        .await
        .unwrap();
    let d: Option<String> = sqlx::query_scalar("SELECT description FROM entities WHERE name = $1")
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        d.as_deref(),
        Some("a database"),
        "a pass that knows no description must not destroy one an earlier pass wrote"
    );
}

/// STUB-COVERAGE GAP (c): `replace_chunk_edges`'s
/// `ON CONFLICT ... DO UPDATE SET weight` clause.
///
/// REACHABILITY, stated honestly, because the clause carries a long comment
/// defending a branch that is much harder to reach than that comment implies:
///
///  - Duplicates WITHIN one call cannot reach it. The client-side dedupe by
///    `(source, target, relation)` runs first, precisely because Postgres
///    refuses to let one statement's ON CONFLICT affect a row twice.
///  - A repeat call for the same `(workspace, chunk, model)` cannot reach it
///    either: the DELETE at the top of the transaction has already removed
///    every row the INSERT could collide with.
///  - Two workspaces extracting the same shared chunk cannot reach it: the
///    PK's `source_entity`/`target_entity` are per-workspace entity ids, so
///    their PK tuples differ.
///
/// What CAN reach it is a row the workspace-scoped DELETE does not cover. The
/// `edges` PK is `(source_entity, target_entity, relation, source_chunk_hash,
/// model_id)` — it does NOT include `workspace_id`, while the DELETE does. So
/// a row carrying the same PK tuple under a different `workspace_id` survives
/// the DELETE and collides on INSERT. This test constructs exactly that,
/// seeding the row directly, and proves the ON CONFLICT clause absorbs it
/// instead of aborting the whole transaction with a PK violation.
///
/// This is defence-in-depth, not a path production reaches today: nothing in
/// `apply_extraction` mixes one workspace's entity ids into another
/// workspace's edge write. It is worth keeping and worth testing, because the
/// PK not including `workspace_id` is exactly the sort of asymmetry that
/// becomes reachable the moment someone adds a caller.
#[tokio::test]
async fn replace_chunk_edges_absorbs_a_conflicting_row_its_delete_did_not_cover() {
    let (pool, ws, page) = setup().await;
    let (_, other_ws, _) = setup().await;
    let model = format!("model-{}", uuid::Uuid::new_v4());
    let h = format!("hash-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "conflicting row text").await;

    let e1 = graph::resolve_entity(&pool, ws, "c1", Some("CONCEPT"), None)
        .await
        .unwrap();
    let e2 = graph::resolve_entity(&pool, ws, "c2", Some("CONCEPT"), None)
        .await
        .unwrap();

    // A row with the SAME PK tuple but a different workspace_id: the
    // workspace-scoped DELETE below will not remove it.
    sqlx::query(
        "INSERT INTO edges
           (source_entity, target_entity, relation, weight, source_chunk_hash, model_id, workspace_id)
         VALUES ($1, $2, 'rel', 0.1, $3, $4, $5)",
    )
    .bind(e1)
    .bind(e2)
    .bind(&h)
    .bind(&model)
    .bind(other_ws)
    .execute(&pool)
    .await
    .unwrap();

    // Without the ON CONFLICT clause this is a PK violation and the whole
    // transaction — edges AND marker — rolls back.
    graph::replace_chunk_edges(&pool, ws, &h, &model, &[(e1, e2, "rel".to_string(), 0.9)])
        .await
        .expect("a colliding row the DELETE could not cover must be absorbed, not abort the write");

    let w: f32 = sqlx::query_scalar(
        "SELECT weight FROM edges
         WHERE source_entity = $1 AND target_entity = $2 AND relation = 'rel'
           AND source_chunk_hash = $3 AND model_id = $4",
    )
    .bind(e1)
    .bind(e2)
    .bind(&h)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(w, 0.9, "the last write's weight must win");

    // And the marker still committed, because the transaction did not abort.
    let marked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chunk_extractions
         WHERE workspace_id = $1 AND content_hash = $2 AND model_id = $3",
    )
    .bind(ws)
    .bind(&h)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(marked, 1);
}

/// ONE DEFINITION OF "LIVE" ACROSS BOTH QUEUES.
///
/// The design defines a live chunk as one referenced by a NON-ARCHIVED page.
/// Neither `pending_extraction`, `extraction_progress`, nor `chunks::pending`
/// filtered `pages.archived_at` — so archiving a page left its chunks in both
/// the embedding and the extraction queue, and the graph/embeddings kept being
/// built for content the user had deleted. Archiving is the product's delete;
/// spending model calls on it is waste.
///
/// SCOPE, STATED HONESTLY: this is a COST fix, not a privacy one. Archiving
/// stops FUTURE work — it does not retract past work. Nothing deletes `edges`
/// or `entities` when a page is archived (the only `DELETE FROM edges` in the
/// codebase is `replace_chunk_edges`'s re-extraction scope), so an archived
/// page's entities and relations persist in the graph in full. And because
/// `extraction_progress` now reports (0, 0) for it, nothing will ever revisit
/// them either. Retracting an archived page's graph needs the same machinery
/// as reaping orphan entities — nothing in the system removes graph nodes or
/// edges at all — and both are recorded together as an M2b-1 prerequisite in
/// `.superpowers/sdd/progress.md`. See also
/// `orphan_entities_survive_an_edit_with_zero_live_edges_a_known_m2b_gap`.
///
/// Notably NOTHING in the M1b suite broke when this filter was added, because
/// nothing covered the semantics at all — the gap was in coverage, not in an
/// M1b test encoding the opposite intent.
#[tokio::test]
async fn archiving_a_page_removes_its_chunks_from_both_queues() {
    let (pool, ws, page) = setup().await;
    let model = format!("model-{}", uuid::Uuid::new_v4());
    let h = format!("hash-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "content that will be archived").await;

    assert!(
        graph::pending_extraction(&pool, &model, Some(ws), 1_000_000)
            .await
            .unwrap()
            .iter()
            .any(|(hash, _)| hash == &h),
        "sanity: a live page's chunk must start out in the extraction queue"
    );
    assert!(
        noted_db::chunks::pending(&pool, &model, Some(ws), 1_000_000)
            .await
            .unwrap()
            .iter()
            .any(|c| c.content_hash == h),
        "sanity: it must start out in the embedding queue too"
    );
    assert_eq!(
        graph::extraction_progress(&pool, &model, Some(ws))
            .await
            .unwrap(),
        (0, 1),
        "sanity: one live chunk, none extracted"
    );

    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(page)
        .execute(&pool)
        .await
        .unwrap();

    assert!(
        !graph::pending_extraction(&pool, &model, Some(ws), 1_000_000)
            .await
            .unwrap()
            .iter()
            .any(|(hash, _)| hash == &h),
        "an archived page's chunk must leave the EXTRACTION queue"
    );
    assert!(
        !noted_db::chunks::pending(&pool, &model, Some(ws), 1_000_000)
            .await
            .unwrap()
            .iter()
            .any(|c| c.content_hash == h),
        "an archived page's chunk must leave the EMBEDDING queue — one definition of live, \
         not two"
    );
    assert_eq!(
        graph::extraction_progress(&pool, &model, Some(ws))
            .await
            .unwrap(),
        (0, 0),
        "an archived chunk must leave the progress denominator too; otherwise it sits there \
         un-drainable, pinning progress below 100% forever"
    );
    assert_eq!(
        noted_db::chunks::progress(&pool, &model, Some(ws))
            .await
            .unwrap(),
        (0, 0),
        "the embedding progress denominator must agree with the extraction one"
    );

    // And the extraction worker must not fan out into a workspace that only
    // reaches the chunk through an archived page — if it did, the fan-out and
    // the queue would disagree about what "live" means and a chunk could be
    // polled forever without ever being markable.
    assert!(
        graph::workspaces_for_chunk(&pool, &h)
            .await
            .unwrap()
            .is_empty(),
        "workspaces_for_chunk must use the SAME definition of live as the queue"
    );
}

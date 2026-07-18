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

    let id1 = graph::resolve_entity(&pool, ws, &normalise("Postgres"), "CONCEPT", None)
        .await
        .unwrap();
    let id2 = graph::resolve_entity(&pool, ws, &normalise("  postgres "), "CONCEPT", None)
        .await
        .unwrap();
    assert_eq!(
        id1, id2,
        "the same normalised name in the same workspace must resolve to the same entity"
    );

    let id3 = graph::resolve_entity(&pool, ws2, &normalise("Postgres"), "CONCEPT", None)
        .await
        .unwrap();
    assert_ne!(
        id1, id3,
        "the same name in a different workspace must be a different entity node"
    );
}

#[tokio::test]
async fn replace_chunk_edges_writes_edges_but_does_not_mark_extracted() {
    let (pool, ws, page) = setup().await;
    let model = format!("model-{}", uuid::Uuid::new_v4());
    let h = format!("hash-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "Alice met Bob").await;

    let alice = graph::resolve_entity(&pool, ws, "alice", "PERSON", None)
        .await
        .unwrap();
    let bob = graph::resolve_entity(&pool, ws, "bob", "PERSON", None)
        .await
        .unwrap();
    let carol = graph::resolve_entity(&pool, ws, "carol", "PERSON", None)
        .await
        .unwrap();

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

    // The marker is now a SEPARATE call (`mark_extracted`) — see its doc
    // comment. `replace_chunk_edges` alone must not set it: a chunk shared
    // across workspaces needs every referencing workspace's edges written
    // before it is truly "done", and the marker has no workspace column to
    // make that partial.
    let extracted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chunk_extractions WHERE content_hash = $1 AND model_id = $2",
    )
    .bind(&h)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        extracted, 0,
        "replace_chunk_edges alone must not set the chunk_extractions marker"
    );

    let pending = graph::pending_extraction(&pool, &model, Some(ws), 1_000_000)
        .await
        .unwrap();
    assert!(
        pending.iter().any(|(hash, _)| hash == &h),
        "a chunk must stay pending until mark_extracted is called explicitly"
    );

    graph::mark_extracted(&pool, &h, &model).await.unwrap();

    let extracted_after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chunk_extractions WHERE content_hash = $1 AND model_id = $2",
    )
    .bind(&h)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(extracted_after, 1, "mark_extracted must set the marker");

    let pending_after = graph::pending_extraction(&pool, &model, Some(ws), 1_000_000)
        .await
        .unwrap();
    assert!(
        !pending_after.iter().any(|(hash, _)| hash == &h),
        "after mark_extracted, the chunk must not still be pending"
    );
}

#[tokio::test]
async fn mark_extracted_is_idempotent() {
    let (pool, _ws, page) = setup().await;
    let model = format!("model-{}", uuid::Uuid::new_v4());
    let h = format!("hash-{}", uuid::Uuid::new_v4());
    live_chunk(&pool, page, &h, "idempotent marker text").await;

    graph::mark_extracted(&pool, &h, &model).await.unwrap();
    graph::mark_extracted(&pool, &h, &model).await.unwrap();

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chunk_extractions WHERE content_hash = $1 AND model_id = $2",
    )
    .bind(&h)
    .bind(&model)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1, "calling mark_extracted twice must not double-insert");
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

    let e1 = graph::resolve_entity(&pool, ws, "e1", "CONCEPT", None)
        .await
        .unwrap();
    let e2 = graph::resolve_entity(&pool, ws, "e2", "CONCEPT", None)
        .await
        .unwrap();

    graph::replace_chunk_edges(&pool, ws, &h_a, &model, &[(e1, e2, "rel-a".to_string(), 1.0)])
        .await
        .unwrap();
    graph::replace_chunk_edges(&pool, ws, &h_b, &model, &[(e1, e2, "rel-b".to_string(), 1.0)])
        .await
        .unwrap();

    // Replacing A's edges again must not touch B's.
    graph::replace_chunk_edges(&pool, ws, &h_a, &model, &[(e2, e1, "rel-a2".to_string(), 1.0)])
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
    graph::mark_extracted(&pool, &h, &model_a).await.unwrap();

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

    let e1 = graph::resolve_entity(&pool, ws, "dup1", "CONCEPT", None)
        .await
        .unwrap();
    let e2 = graph::resolve_entity(&pool, ws, "dup2", "CONCEPT", None)
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

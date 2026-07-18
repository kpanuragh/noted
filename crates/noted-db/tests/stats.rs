use noted_db::{graph, stats};

/// Every fixture below builds TWO fully-populated workspaces and asserts against
/// one of them. Tenancy leaks are the defect class this project has produced
/// most, and a stat that quietly counts the whole instance looks perfectly
/// healthy until someone else's data is in your dashboard.
async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('stats-test') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    (pool, ws)
}

/// A unique model id per test. Required, not cosmetic: `embeddings` is keyed
/// `(content_hash, model_id)` GLOBALLY, so a shared model id would let one
/// test's vectors satisfy another's "is it embedded" check. The M2a ledger
/// records this as a standing rule — a test must isolate its own DATA SPACE,
/// not merely its rows.
fn model_id() -> String {
    format!("stats-test-{}", uuid::Uuid::new_v4())
}

async fn page(pool: &noted_db::PgPool, ws: uuid::Uuid, title: &str) -> uuid::Uuid {
    sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
        .bind(ws)
        .bind(title)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn archive(pool: &noted_db::PgPool, id: uuid::Uuid) {
    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

/// A chunk linked to a page. `hash` is globally content-addressed, so tests that
/// want distinct chunks must pass distinct hashes.
async fn live_chunk(pool: &noted_db::PgPool, page_id: uuid::Uuid, hash: &str) {
    noted_db::chunks::upsert(pool, &[(hash.to_string(), format!("text for {hash}"), 10)])
        .await
        .unwrap();
    let mut all: Vec<String> = sqlx::query_scalar(
        "SELECT content_hash FROM page_chunks WHERE page_id = $1 ORDER BY chunk_index",
    )
    .bind(page_id)
    .fetch_all(pool)
    .await
    .unwrap();
    all.push(hash.to_string());
    noted_db::chunks::set_page_chunks(pool, page_id, &all)
        .await
        .unwrap();
}

async fn embed(pool: &noted_db::PgPool, hash: &str, model: &str) {
    noted_db::chunks::store_embedding(pool, hash, model, &vec![0.1_f32; 768])
        .await
        .unwrap();
}

/// Build a workspace with a known shape: `pages` live pages (plus one archived),
/// `embedded` embedded chunks and one unembedded chunk, and one edge between two
/// entities. Returns the workspace id.
async fn populate(pool: &noted_db::PgPool, ws: uuid::Uuid, model: &str, tag: &str) {
    let p = page(pool, ws, "live one").await;
    let _ = page(pool, ws, "live two").await;
    let gone = page(pool, ws, "archived").await;
    archive(pool, gone).await;

    live_chunk(pool, p, &format!("{tag}-embedded")).await;
    live_chunk(pool, p, &format!("{tag}-unembedded")).await;
    embed(pool, &format!("{tag}-embedded"), model).await;

    let a = graph::resolve_entity(pool, ws, "alice", Some("PERSON"), None)
        .await
        .unwrap();
    let b = graph::resolve_entity(pool, ws, "bob", Some("PERSON"), None)
        .await
        .unwrap();
    graph::replace_chunk_edges(
        pool,
        ws,
        &format!("{tag}-embedded"),
        model,
        &[(a, b, "knows".to_string(), 1.0)],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn counts_are_correct() {
    let (pool, ws) = setup().await;
    let model = model_id();
    populate(&pool, ws, &model, "ok").await;

    let s = stats::workspace_stats(&pool, ws, &model).await.unwrap();
    assert_eq!(
        s.pages, 2,
        "only LIVE pages count; the archived one must not"
    );
    assert_eq!(
        s.chunks_indexed, 1,
        "only the chunk that actually has an embedding counts"
    );
    assert_eq!(s.entities, 2);
    assert_eq!(s.edges, 1);
}

/// THE test that matters. A second workspace with its own full set of rows must
/// contribute nothing. Build B first and assert on A, so a leak shows up as an
/// inflated count rather than a coincidentally-equal one.
#[tokio::test]
async fn every_stat_is_workspace_scoped() {
    let (pool, ws_a) = setup().await;
    let (_, ws_b) = setup().await;
    let model = model_id();

    populate(&pool, ws_b, &model, "tenant-b").await;
    populate(&pool, ws_a, &model, "tenant-a").await;

    let a = stats::workspace_stats(&pool, ws_a, &model).await.unwrap();
    assert_eq!(a.pages, 2, "another tenant's pages must not be counted");
    assert_eq!(
        a.chunks_indexed, 1,
        "another tenant's indexed chunks must not be counted"
    );
    assert_eq!(
        a.entities, 2,
        "another tenant's entities must not be counted"
    );
    assert_eq!(a.edges, 1, "another tenant's edges must not be counted");

    // And symmetrically, so a query that somehow returned only ws_a's rows for
    // both callers would still be caught.
    let b = stats::workspace_stats(&pool, ws_b, &model).await.unwrap();
    assert_eq!(b.pages, 2);
    assert_eq!(b.chunks_indexed, 1);
    assert_eq!(b.entities, 2);
    assert_eq!(b.edges, 1);
}

/// `chunks_indexed` must mean "indexed under the model we are actually
/// searching with". `embeddings` is keyed `(content_hash, model_id)` and several
/// models' vectors coexist by design, so counting any-model would report a
/// workspace as searchable when the active model has no vectors for it at all.
#[tokio::test]
async fn chunks_indexed_is_per_model() {
    let (pool, ws) = setup().await;
    let old_model = model_id();
    let new_model = model_id();
    populate(&pool, ws, &old_model, "per-model").await;

    let s = stats::workspace_stats(&pool, ws, &new_model).await.unwrap();
    assert_eq!(
        s.chunks_indexed, 0,
        "a chunk embedded only under a DIFFERENT model is not indexed for this one"
    );
    assert_eq!(s.pages, 2, "the other stats do not depend on the model");
}

/// Archiving is this product's delete. An archived page's chunks must drop out
/// of `chunks_indexed` exactly as they drop out of both work queues — the same
/// one definition of "live" shared by chunks::pending, chunks::progress,
/// graph::pending_extraction and pages::all_page_ids.
#[tokio::test]
async fn archiving_a_page_removes_its_chunks_from_the_indexed_count() {
    let (pool, ws) = setup().await;
    let model = model_id();
    let p = page(&pool, ws, "doomed").await;
    live_chunk(&pool, p, "archive-me").await;
    embed(&pool, "archive-me", &model).await;

    let before = stats::workspace_stats(&pool, ws, &model).await.unwrap();
    assert_eq!(before.chunks_indexed, 1);
    assert_eq!(before.pages, 1);

    archive(&pool, p).await;

    let after = stats::workspace_stats(&pool, ws, &model).await.unwrap();
    assert_eq!(after.pages, 0, "an archived page is not a live page");
    assert_eq!(
        after.chunks_indexed, 0,
        "an archived page's chunks are not indexed content"
    );
}

/// Content addressing means two workspaces legitimately SHARE a chunk row when
/// their text is byte-identical. Each must count it once, for itself.
#[tokio::test]
async fn a_chunk_shared_between_workspaces_counts_once_for_each() {
    let (pool, ws_a) = setup().await;
    let (_, ws_b) = setup().await;
    let model = model_id();
    let shared = format!("shared-{}", uuid::Uuid::new_v4());

    let pa = page(&pool, ws_a, "a").await;
    let pb = page(&pool, ws_b, "b").await;
    live_chunk(&pool, pa, &shared).await;
    live_chunk(&pool, pb, &shared).await;
    embed(&pool, &shared, &model).await;

    let a = stats::workspace_stats(&pool, ws_a, &model).await.unwrap();
    let b = stats::workspace_stats(&pool, ws_b, &model).await.unwrap();
    assert_eq!(a.chunks_indexed, 1);
    assert_eq!(b.chunks_indexed, 1);
}

/// Two pages in ONE workspace sharing a chunk must count it once, not twice —
/// `chunks_indexed` is a DISTINCT count of chunks, not of page-chunk links.
#[tokio::test]
async fn a_chunk_on_two_pages_in_one_workspace_counts_once() {
    let (pool, ws) = setup().await;
    let model = model_id();
    let shared = format!("dup-{}", uuid::Uuid::new_v4());

    let p1 = page(&pool, ws, "one").await;
    let p2 = page(&pool, ws, "two").await;
    live_chunk(&pool, p1, &shared).await;
    live_chunk(&pool, p2, &shared).await;
    embed(&pool, &shared, &model).await;

    let s = stats::workspace_stats(&pool, ws, &model).await.unwrap();
    assert_eq!(
        s.chunks_indexed, 1,
        "one chunk on two pages is one indexed chunk"
    );
}

#[tokio::test]
async fn an_empty_workspace_reports_zeroes_not_an_error() {
    let (pool, ws) = setup().await;
    let s = stats::workspace_stats(&pool, ws, &model_id())
        .await
        .unwrap();
    assert_eq!(
        (s.pages, s.chunks_indexed, s.entities, s.edges),
        (0, 0, 0, 0)
    );
}

//! M6-5 — reaping graph residue.
use noted_db::graph;
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

async fn workspace(pool: &noted_db::PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('reap-test') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A live page owning one chunk, with an edge extracted from it.
async fn page_with_edge(
    pool: &noted_db::PgPool,
    ws: Uuid,
    model: &str,
    label: &str,
) -> (Uuid, String, Uuid, Uuid) {
    let page: Uuid =
        sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
            .bind(ws)
            .bind(label)
            .fetch_one(pool)
            .await
            .unwrap();
    let hash = format!("reap-{}", Uuid::new_v4());
    noted_db::chunks::upsert(pool, &[(hash.clone(), format!("text for {label}"), 10)])
        .await
        .unwrap();
    noted_db::chunks::set_page_chunks(pool, page, &[hash.clone()])
        .await
        .unwrap();

    let a = graph::resolve_entity(pool, ws, &format!("{label}-a-{}", Uuid::new_v4()), Some("CONCEPT"), None)
        .await
        .unwrap();
    let b = graph::resolve_entity(pool, ws, &format!("{label}-b-{}", Uuid::new_v4()), Some("CONCEPT"), None)
        .await
        .unwrap();
    graph::replace_chunk_edges(pool, ws, &hash, model, &[(a, b, "rel".into(), 1.0)])
        .await
        .unwrap();

    (page, hash, a, b)
}

async fn edge_count(pool: &noted_db::PgPool, ws: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM edges WHERE workspace_id = $1")
        .bind(ws)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn entity_count(pool: &noted_db::PgPool, ws: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM entities WHERE workspace_id = $1")
        .bind(ws)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// **The headline property: archiving a page retracts its graph contribution.**
///
/// Until now nothing in the system removed a graph node or edge, so an archived
/// page — the product's delete — left its entities and relations in place
/// forever.
///
/// MECHANISM PROTECTED: the `archived_at IS NULL` predicate in
/// `reap_dead_edges`. Remove it and the edge survives archiving, which is the
/// whole bug.
#[tokio::test]
async fn archiving_a_page_retracts_the_graph_it_contributed() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = format!("reap-{}", Uuid::new_v4());
    let (page, _hash, _a, _b) = page_with_edge(&pool, ws, &model, "doomed").await;

    // Premise: the graph exists before we archive anything.
    assert_eq!(edge_count(&pool, ws).await, 1);
    assert_eq!(entity_count(&pool, ws).await, 2);

    // A sweep BEFORE archiving must remove nothing — otherwise the assertion
    // after archiving would prove only that the reaper deletes indiscriminately.
    let noop = graph::reap_graph(&pool, Some(ws)).await.unwrap();
    assert_eq!(
        (noop.edges, noop.entities),
        (0, 0),
        "a live page's graph must survive a sweep"
    );

    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(page)
        .execute(&pool)
        .await
        .unwrap();

    let reaped = graph::reap_graph(&pool, Some(ws)).await.unwrap();
    assert_eq!(reaped.edges, 1, "the archived page's edge must go");
    assert_eq!(reaped.entities, 2, "and the entities it orphaned with it");
    assert_eq!(edge_count(&pool, ws).await, 0);
    assert_eq!(entity_count(&pool, ws).await, 0);
}

/// Ordering is load-bearing: entities are orphaned BY the edge sweep, so
/// reaping entities first collects nothing.
///
/// MECHANISM PROTECTED: the sequence inside `reap_graph`. Swap the two calls and
/// this fails — the entity sweep runs while the edges still name them, finds
/// nothing, and the orphans survive until some later sweep.
#[tokio::test]
async fn entities_are_reaped_after_the_edges_that_orphan_them_not_before() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = format!("reap-{}", Uuid::new_v4());
    let (page, _h, _a, _b) = page_with_edge(&pool, ws, &model, "order").await;

    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(page)
        .execute(&pool)
        .await
        .unwrap();

    // ONE sweep must finish the job. If entities were swept first they would
    // still be edge-referenced at that moment, and this would report 0.
    let reaped = graph::reap_graph(&pool, Some(ws)).await.unwrap();
    assert_eq!(
        (reaped.edges, reaped.entities),
        (1, 2),
        "a single sweep must collect both the edge and the entities it orphans"
    );
}

/// A chunk shared with a LIVE page keeps its edges — even though another page
/// referencing the same text was archived.
///
/// This is the case that makes the naive query wrong: chunks are
/// content-addressed and shared, so "this page is archived" does not mean "this
/// chunk is dead".
#[tokio::test]
async fn a_chunk_still_referenced_by_a_live_page_keeps_its_edges() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = format!("reap-{}", Uuid::new_v4());
    let (archived, hash, _a, _b) = page_with_edge(&pool, ws, &model, "shared").await;

    // A second, LIVE page holding the very same chunk.
    let live: Uuid =
        sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, 'Live') RETURNING id")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .unwrap();
    noted_db::chunks::set_page_chunks(&pool, live, &[hash.clone()])
        .await
        .unwrap();

    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(archived)
        .execute(&pool)
        .await
        .unwrap();

    let reaped = graph::reap_graph(&pool, Some(ws)).await.unwrap();
    assert_eq!(
        (reaped.edges, reaped.entities),
        (0, 0),
        "the chunk is still live via another page; its graph must stay"
    );
    assert_eq!(edge_count(&pool, ws).await, 1);
}

/// Another tenant's live page must NOT keep this tenant's edges alive.
///
/// Chunks are global and content-addressed, so a liveness test that asked "does
/// any live page reference this chunk" would be answered by a stranger's page.
///
/// MECHANISM PROTECTED: `p.workspace_id = e.workspace_id` in the EXISTS clause.
/// Remove it and this fails — the fourth instance of the bug class this project
/// keeps producing.
#[tokio::test]
async fn another_tenants_live_page_does_not_keep_your_edges_alive() {
    let pool = pool().await;
    let mine = workspace(&pool).await;
    let theirs = workspace(&pool).await;
    let model = format!("reap-{}", Uuid::new_v4());

    let (my_page, hash, _a, _b) = page_with_edge(&pool, mine, &model, "mine").await;

    // Their page holds byte-identical text, so it shares the very same chunk.
    let their_page: Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, title) VALUES ($1, 'Theirs') RETURNING id",
    )
    .bind(theirs)
    .fetch_one(&pool)
    .await
    .unwrap();
    noted_db::chunks::set_page_chunks(&pool, their_page, &[hash.clone()])
        .await
        .unwrap();

    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(my_page)
        .execute(&pool)
        .await
        .unwrap();

    let reaped = graph::reap_graph(&pool, Some(mine)).await.unwrap();
    assert_eq!(
        reaped.edges, 1,
        "my only live page is archived, so my edges are dead regardless of \
         whose page still holds the chunk"
    );
    assert_eq!(edge_count(&pool, mine).await, 0);
}

/// The sweep is scoped: reaping one workspace cannot touch another's graph.
#[tokio::test]
async fn reaping_one_workspace_leaves_another_alone() {
    let pool = pool().await;
    let a = workspace(&pool).await;
    let b = workspace(&pool).await;
    let model = format!("reap-{}", Uuid::new_v4());

    let (a_page, _, _, _) = page_with_edge(&pool, a, &model, "a").await;
    let (b_page, _, _, _) = page_with_edge(&pool, b, &model, "b").await;

    // BOTH are archived, so only the scope decides what is collected.
    for p in [a_page, b_page] {
        sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
            .bind(p)
            .execute(&pool)
            .await
            .unwrap();
    }

    let reaped = graph::reap_graph(&pool, Some(a)).await.unwrap();
    assert_eq!(reaped.edges, 1, "only A's edge");
    assert_eq!(edge_count(&pool, a).await, 0);
    assert_eq!(edge_count(&pool, b).await, 1, "B's graph must be untouched");
}

/// Running the sweep twice collects nothing the second time — it is idempotent,
/// so a scheduler can run it on a timer without doing damage.
#[tokio::test]
async fn a_second_sweep_finds_nothing_left_to_do() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let model = format!("reap-{}", Uuid::new_v4());
    let (page, _, _, _) = page_with_edge(&pool, ws, &model, "twice").await;

    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(page)
        .execute(&pool)
        .await
        .unwrap();

    let first = graph::reap_graph(&pool, Some(ws)).await.unwrap();
    assert_eq!((first.edges, first.entities), (1, 2));

    let second = graph::reap_graph(&pool, Some(ws)).await.unwrap();
    assert_eq!((second.edges, second.entities), (0, 0));
}

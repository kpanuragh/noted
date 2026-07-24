//! M3-5 — databases feed the knowledge graph.
use noted_db::collections::{self, PropertyKind};
use noted_db::{property_graph, relations};
use serde_json::json;
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let p = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&p).await.unwrap();
    p
}
async fn ws(p: &noted_db::PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('pg-test') RETURNING id")
        .fetch_one(p).await.unwrap()
}
async fn page(p: &noted_db::PgPool, w: Uuid, t: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1,$2) RETURNING id")
        .bind(w).bind(t).fetch_one(p).await.unwrap()
}
async fn edges(p: &noted_db::PgPool, w: Uuid) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT s.name, pe.relation, t.name
         FROM property_edges pe
         JOIN entities s ON s.id = pe.source_entity
         JOIN entities t ON t.id = pe.target_entity
         WHERE pe.workspace_id = $1 ORDER BY pe.relation, t.name")
        .bind(w).fetch_all(p).await.unwrap()
}

async fn fixture() -> (noted_db::PgPool, Uuid, Uuid, Uuid, Uuid) {
    let pool = pool().await;
    let w = ws(&pool).await;
    let host = page(&pool, w, "Tasks").await;
    let c = collections::create_collection(&pool, w, host, "Tasks").await.unwrap();
    let status = collections::add_property(&pool, c.id, "Status", PropertyKind::Select,
        json!({"options":["blocked","done"]}), 0).await.unwrap().id;
    let tags = collections::add_property(&pool, c.id, "Tags", PropertyKind::MultiSelect, json!({}), 1)
        .await.unwrap().id;
    (pool, w, c.id, status, tags)
}

/// **A select value becomes a graph edge**, in the user's own vocabulary.
#[tokio::test]
async fn a_select_value_becomes_an_edge_named_after_its_property() {
    let (pool, w, _c, status, _t) = fixture().await;
    let task = page(&pool, w, "Ship auth").await;
    collections::set_value(&pool, task, status, json!("blocked")).await.unwrap();

    let n = property_graph::reproject_page(&pool, w, task).await.unwrap();
    assert_eq!(n, 1);
    assert_eq!(edges(&pool, w).await,
        vec![("ship auth".into(), "status".into(), "blocked".into())],
        "the relation is the property's own name, not a generic has_value");
}

/// Multi-select contributes one edge per option.
#[tokio::test]
async fn a_multi_select_contributes_one_edge_per_option() {
    let (pool, w, _c, _s, tags) = fixture().await;
    let task = page(&pool, w, "Ship auth").await;
    collections::set_value(&pool, task, tags, json!(["security","backend"])).await.unwrap();

    property_graph::reproject_page(&pool, w, task).await.unwrap();
    let got = edges(&pool, w).await;
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|(_, r, _)| r == "tags"));
}

/// **A relation joins two PAGE entities**, which is what makes "blocked by"
/// traversable in the graph rather than only in the table.
#[tokio::test]
async fn a_relation_joins_the_two_pages_it_links() {
    let (pool, w, c, _s, _t) = fixture().await;
    let rel = collections::add_property(&pool, c, "Blocked by", PropertyKind::Relation, json!({}), 2)
        .await.unwrap().id;
    let a = page(&pool, w, "Ship auth").await;
    let b = page(&pool, w, "Migrate database").await;
    relations::link(&pool, rel, a, b).await.unwrap();

    property_graph::reproject_page(&pool, w, a).await.unwrap();
    assert_eq!(edges(&pool, w).await,
        vec![("ship auth".into(), "blocked_by".into(), "migrate database".into())],
        "spaces in a property name become underscores in the relation");
}

/// **Numbers and dates do NOT become edges.**
///
/// An edge to the entity "5" would join every five-point task into a cluster
/// that means nothing. Measurements are not things to connect to.
#[tokio::test]
async fn numbers_and_dates_do_not_become_edges() {
    let (pool, w, c, _s, _t) = fixture().await;
    let points = collections::add_property(&pool, c, "Points", PropertyKind::Number, json!({}), 3)
        .await.unwrap().id;
    let due = collections::add_property(&pool, c, "Due", PropertyKind::Date, json!({}), 4)
        .await.unwrap().id;
    let task = page(&pool, w, "Ship auth").await;
    collections::set_value(&pool, task, points, json!(5.0)).await.unwrap();
    collections::set_value(&pool, task, due, json!("2026-07-22")).await.unwrap();

    let n = property_graph::reproject_page(&pool, w, task).await.unwrap();
    assert_eq!(n, 0, "a measurement is not a thing to connect to");
    assert!(edges(&pool, w).await.is_empty());
}

/// **Editing a value replaces the edge rather than accumulating.**
///
/// MECHANISM PROTECTED: the DELETE at the top of `reproject_page`. Without it a
/// task edited from "blocked" to "done" asserts both, forever.
#[tokio::test]
async fn editing_a_value_replaces_the_edge_it_derived() {
    let (pool, w, _c, status, _t) = fixture().await;
    let task = page(&pool, w, "Ship auth").await;

    collections::set_value(&pool, task, status, json!("blocked")).await.unwrap();
    property_graph::reproject_page(&pool, w, task).await.unwrap();
    collections::set_value(&pool, task, status, json!("done")).await.unwrap();
    property_graph::reproject_page(&pool, w, task).await.unwrap();

    let got = edges(&pool, w).await;
    assert_eq!(got.len(), 1, "the old assertion must be gone: {got:?}");
    assert_eq!(got[0].2, "done");
}

/// **Deleting a property retracts its edges** — by CASCADE.
#[tokio::test]
async fn deleting_a_property_retracts_its_edges() {
    let (pool, w, _c, status, tags) = fixture().await;
    let task = page(&pool, w, "Ship auth").await;
    collections::set_value(&pool, task, status, json!("blocked")).await.unwrap();
    collections::set_value(&pool, task, tags, json!(["backend"])).await.unwrap();
    property_graph::reproject_page(&pool, w, task).await.unwrap();
    assert_eq!(edges(&pool, w).await.len(), 2);

    collections::delete_property(&pool, status).await.unwrap();
    let got = edges(&pool, w).await;
    assert_eq!(got.len(), 1, "only the surviving property's edge remains");
    assert_eq!(got[0].1, "tags");
}

/// An archived page contributes nothing, and reprojecting it retracts what it
/// had — the same rule chunk-derived edges follow.
#[tokio::test]
async fn archiving_a_page_retracts_its_property_edges() {
    let (pool, w, _c, status, _t) = fixture().await;
    let task = page(&pool, w, "Ship auth").await;
    collections::set_value(&pool, task, status, json!("blocked")).await.unwrap();
    property_graph::reproject_page(&pool, w, task).await.unwrap();
    assert_eq!(property_graph::count(&pool, w).await.unwrap(), 1);

    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(task).execute(&pool).await.unwrap();
    let n = property_graph::reproject_page(&pool, w, task).await.unwrap();
    assert_eq!(n, 0);
    assert!(edges(&pool, w).await.is_empty());
}

/// **A property value and a mention in prose resolve to the SAME entity.**
///
/// MECHANISM PROTECTED: `normalise_for_graph` matching the extractor's
/// normalisation. Without it "Helios" from a select and "helios" from a
/// sentence are two nodes, and the graph shows two unrelated things where the
/// user sees one.
#[tokio::test]
async fn a_property_value_and_a_prose_mention_are_one_entity() {
    let (pool, w, _c, status, _t) = fixture().await;
    let task = page(&pool, w, "Ship auth").await;
    collections::set_value(&pool, task, status, json!("Blocked")).await.unwrap();
    property_graph::reproject_page(&pool, w, task).await.unwrap();

    // The extractor would have resolved the lowercase form.
    let from_prose = noted_db::graph::resolve_entity(&pool, w, "blocked", Some("CONCEPT"), None)
        .await.unwrap();
    let from_property: Uuid = sqlx::query_scalar(
        "SELECT target_entity FROM property_edges WHERE page_id = $1")
        .bind(task).fetch_one(&pool).await.unwrap();

    assert_eq!(from_prose, from_property, "one entity, not two");
}

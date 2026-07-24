//! M3-4 — relations and rollups.
use noted_db::collections::{self, PropertyKind};
use noted_db::relations::{self, RelationError};
use serde_json::json;
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let p = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&p).await.unwrap();
    p
}
async fn ws(pool: &noted_db::PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('rel-test') RETURNING id")
        .fetch_one(pool).await.unwrap()
}
async fn page(pool: &noted_db::PgPool, w: Uuid, title: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
        .bind(w).bind(title).fetch_one(pool).await.unwrap()
}
async fn coll(pool: &noted_db::PgPool, w: Uuid, name: &str) -> Uuid {
    let host = page(pool, w, name).await;
    collections::create_collection(pool, w, host, name).await.unwrap().id
}

/// Relations are bidirectional: both directions are answerable, and neither
/// requires scanning.
#[tokio::test]
async fn a_relation_is_visible_from_both_ends() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let c = coll(&pool, w, "Tasks").await;
    let rel = collections::add_property(&pool, c, "Project", PropertyKind::Relation, json!({}), 0)
        .await.unwrap().id;

    let task = page(&pool, w, "Task").await;
    let project = page(&pool, w, "Project").await;
    relations::link(&pool, rel, task, project).await.unwrap();

    assert_eq!(relations::forward(&pool, rel, task).await.unwrap(), vec![project]);
    assert_eq!(relations::backward(&pool, rel, project).await.unwrap(), vec![task],
        "the other side must be answerable without scanning every value");
}

/// **A relation must point at a live page in the same workspace.**
///
/// MECHANISM PROTECTED: the target check in `link`. Remove it and all three
/// cases below store happily, and render as broken rows forever.
#[tokio::test]
async fn relations_are_checked_against_real_pages() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let other = ws(&pool).await;
    let c = coll(&pool, w, "Tasks").await;
    let rel = collections::add_property(&pool, c, "Project", PropertyKind::Relation, json!({}), 0)
        .await.unwrap().id;
    let task = page(&pool, w, "Task").await;

    // A good link works, so a blanket rejection cannot pass this test.
    let good = page(&pool, w, "Good").await;
    relations::link(&pool, rel, task, good).await.unwrap();

    let nowhere = Uuid::new_v4();
    assert!(matches!(relations::link(&pool, rel, task, nowhere).await,
        Err(RelationError::BadTarget)), "a uuid naming nothing");

    let foreign = page(&pool, other, "Theirs").await;
    assert!(matches!(relations::link(&pool, rel, task, foreign).await,
        Err(RelationError::BadTarget)), "another tenant's page");

    let archived = page(&pool, w, "Archived").await;
    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(archived).execute(&pool).await.unwrap();
    assert!(matches!(relations::link(&pool, rel, task, archived).await,
        Err(RelationError::BadTarget)), "an archived page");
}

/// Deleting either side removes the link — by CASCADE, so no application code
/// can forget it.
#[tokio::test]
async fn deleting_either_side_removes_the_link() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let c = coll(&pool, w, "Tasks").await;
    let rel = collections::add_property(&pool, c, "Project", PropertyKind::Relation, json!({}), 0)
        .await.unwrap().id;
    let task = page(&pool, w, "Task").await;
    let project = page(&pool, w, "Project").await;
    relations::link(&pool, rel, task, project).await.unwrap();

    sqlx::query("DELETE FROM pages WHERE id = $1").bind(project).execute(&pool).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM page_relations WHERE property_id = $1")
        .bind(rel).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
}

/// Sets up a project with three tasks carrying points, and a rollup over them.
async fn rollup_fixture(func: &str) -> (noted_db::PgPool, Uuid, Uuid, Uuid, Uuid, Vec<Uuid>) {
    let pool = pool().await;
    let w = ws(&pool).await;
    let tasks = coll(&pool, w, "Tasks").await;
    let projects = coll(&pool, w, "Projects").await;

    let points = collections::add_property(&pool, tasks, "Points", PropertyKind::Number, json!({}), 0)
        .await.unwrap().id;
    let rel = collections::add_property(&pool, projects, "Tasks", PropertyKind::Relation, json!({}), 0)
        .await.unwrap().id;
    let rollup = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO collection_properties (collection_id, name, kind, config, position)
         VALUES ($1, $2, 'rollup', $3, 1) RETURNING id")
        .bind(projects).bind(format!("{func} of points"))
        .bind(json!({"via": rel, "target": points, "function": func}))
        .fetch_one(&pool).await.unwrap();

    let project = page(&pool, w, "Project").await;
    let mut task_ids = Vec::new();
    for (i, p) in [1.0, 5.0, 3.0].into_iter().enumerate() {
        let t = page(&pool, w, &format!("Task {i}")).await;
        collections::set_value(&pool, t, points, json!(p)).await.unwrap();
        relations::link(&pool, rel, project, t).await.unwrap();
        task_ids.push(t);
    }
    (pool, w, project, rollup, points, task_ids)
}

/// Every aggregate computes what it says.
#[tokio::test]
async fn each_rollup_function_computes_correctly() {
    for (func, expected) in [
        ("count", json!(3)),
        ("sum", json!(9.0)),
        ("min", json!(1.0)),
        ("max", json!(5.0)),
    ] {
        let (pool, _w, project, rollup, _pts, _t) = rollup_fixture(func).await;
        let got = relations::recompute(&pool, rollup, project).await.unwrap();
        assert_eq!(got, expected, "{func}");
    }
}

/// **A rollup recomputes when the RELATED set changes — and the test asserts
/// the recompute happened, not merely that some number is present.**
///
/// MECHANISM PROTECTED: `recompute_for_target`, which walks BACKWARD from the
/// changed row to the rows whose rollups depend on it. That direction is the
/// one that is easy to forget: the project is not reachable from the task by
/// any forward link.
#[tokio::test]
async fn changing_a_related_row_updates_the_rollup() {
    let (pool, w, project, rollup, points, tasks) = rollup_fixture("sum").await;

    let before = relations::recompute(&pool, rollup, project).await.unwrap();
    assert_eq!(before, json!(9.0), "premise");

    // Change a task's points, then let the dependency walk find the project.
    collections::set_value(&pool, tasks[0], points, json!(100.0)).await.unwrap();
    let n = relations::recompute_for_target(&pool, tasks[0]).await.unwrap();
    assert_eq!(n, 1, "exactly one dependent rollup must have been recomputed");

    let stored: serde_json::Value = sqlx::query_scalar(
        "SELECT value FROM page_properties WHERE page_id = $1 AND property_id = $2")
        .bind(project).bind(rollup).fetch_one(&pool).await.unwrap();
    assert_eq!(stored, json!(108.0), "the STORED value must have been updated");

    // Adding a new related row moves it again.
    let extra = page(&pool, w, "Extra").await;
    collections::set_value(&pool, extra, points, json!(2.0)).await.unwrap();
    let rel: Uuid = sqlx::query_scalar("SELECT (config ->> 'via')::uuid FROM collection_properties WHERE id = $1")
        .bind(rollup).fetch_one(&pool).await.unwrap();
    relations::link(&pool, rel, project, extra).await.unwrap();
    relations::recompute(&pool, rollup, project).await.unwrap();

    let after: serde_json::Value = sqlx::query_scalar(
        "SELECT value FROM page_properties WHERE page_id = $1 AND property_id = $2")
        .bind(project).bind(rollup).fetch_one(&pool).await.unwrap();
    assert_eq!(after, json!(110.0));
}

/// An empty relation rolls up to 0 for count and NULL for the rest — zero would
/// be a value the data never contained.
#[tokio::test]
async fn an_empty_relation_rolls_up_to_zero_or_null() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let tasks = coll(&pool, w, "Tasks").await;
    let projects = coll(&pool, w, "Projects").await;
    let points = collections::add_property(&pool, tasks, "Points", PropertyKind::Number, json!({}), 0)
        .await.unwrap().id;
    let rel = collections::add_property(&pool, projects, "Tasks", PropertyKind::Relation, json!({}), 0)
        .await.unwrap().id;
    let lonely = page(&pool, w, "Lonely").await;

    for (func, expected) in [("count", json!(0)), ("sum", json!(null)), ("min", json!(null))] {
        let rollup = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO collection_properties (collection_id, name, kind, config, position)
             VALUES ($1, $2, 'rollup', $3, 9) RETURNING id")
            .bind(projects).bind(format!("empty-{func}"))
            .bind(json!({"via": rel, "target": points, "function": func}))
            .fetch_one(&pool).await.unwrap();
        let got = relations::recompute(&pool, rollup, lonely).await.unwrap();
        assert_eq!(got, expected, "{func} over nothing");
    }
}

/// **A relation cycle does not hang the rollup.**
///
/// A relates to B relates to A is a legitimate thing for a user to build, and a
/// dependency walk without a depth bound would follow it forever.
#[tokio::test]
async fn a_relation_cycle_does_not_hang_the_rollup() {
    let (pool, _w, project, rollup, _points, tasks) = rollup_fixture("count").await;
    let rel: Uuid = sqlx::query_scalar("SELECT (config ->> 'via')::uuid FROM collection_properties WHERE id = $1")
        .bind(rollup).fetch_one(&pool).await.unwrap();

    // Close the loop: a task points back at the project.
    relations::link(&pool, rel, tasks[0], project).await.unwrap();

    let done = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        relations::recompute_for_target(&pool, tasks[0]),
    )
    .await;
    assert!(done.is_ok(), "a cycle must terminate, not spin");
    assert!(done.unwrap().is_ok());
}

/// An unknown rollup function is an error rather than a silent zero.
#[tokio::test]
async fn an_unknown_rollup_function_is_refused() {
    let (pool, _w, project, rollup, points, _t) = rollup_fixture("sum").await;
    // Scoped to THIS fixture's own rollup, not `WHERE kind = 'rollup' LIMIT 1`.
    // That instance-wide form passed alone and failed in the full suite, because
    // it picked up another test's property — the exact hazard this suite has a
    // scar for.
    let projects: Uuid = sqlx::query_scalar(
        "SELECT collection_id FROM collection_properties WHERE id = $1")
        .bind(rollup).fetch_one(&pool).await.unwrap();
    let bad = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO collection_properties (collection_id, name, kind, config, position)
         VALUES ($1, 'bad', 'rollup', $2, 5) RETURNING id")
        .bind(projects)
        .bind(json!({"via": Uuid::new_v4(), "target": points, "function": "median"}))
        .fetch_one(&pool).await.unwrap();

    let err = relations::recompute(&pool, bad, project).await.unwrap_err();
    assert!(matches!(err, RelationError::UnknownFunction(_)), "got {err}");
}

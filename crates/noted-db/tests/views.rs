//! M3-2 — table views, with filters and sorts compiled to SQL.
use noted_db::collections::{self, PropertyKind};
use noted_db::views::{self, ViewError};
use serde_json::json;
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

struct Fixture {
    pool: noted_db::PgPool,
    ws: Uuid,
    user: Uuid,
    collection: Uuid,
    status: Uuid,
    points: Uuid,
    tags: Uuid,
}

/// A collection of tasks with a select, a number and a multi-select, plus three
/// rows — enough that a filter can be wrong in an observable way.
async fn fixture() -> Fixture {
    let pool = pool().await;
    let ws: Uuid = sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('view-test') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    let email = format!("v{}@example.com", Uuid::new_v4().simple());
    let user = noted_db::users::create(&pool, &email, "hash", "V").await.unwrap().id;

    let host: Uuid = sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, 'Tasks') RETURNING id")
        .bind(ws).fetch_one(&pool).await.unwrap();
    let c = collections::create_collection(&pool, ws, host, "Tasks").await.unwrap();

    let status = collections::add_property(&pool, c.id, "Status", PropertyKind::Select, json!({}), 0).await.unwrap().id;
    let points = collections::add_property(&pool, c.id, "Points", PropertyKind::Number, json!({}), 1).await.unwrap().id;
    let tags = collections::add_property(&pool, c.id, "Tags", PropertyKind::MultiSelect, json!({}), 2).await.unwrap().id;

    for (title, st, pts, tg) in [
        ("Ship auth", "done", 5.0, json!(["backend", "security"])),
        ("Table view", "doing", 3.0, json!(["backend"])),
        ("Write docs", "todo", 1.0, json!(["docs"])),
    ] {
        let row: Uuid = sqlx::query_scalar(
            "INSERT INTO pages (workspace_id, parent_id, title) VALUES ($1, $2, $3) RETURNING id")
            .bind(ws).bind(host).bind(title).fetch_one(&pool).await.unwrap();
        collections::set_value(&pool, row, status, json!(st)).await.unwrap();
        collections::set_value(&pool, row, points, json!(pts)).await.unwrap();
        collections::set_value(&pool, row, tags, tg).await.unwrap();
    }

    Fixture { pool, ws, user, collection: c.id, status, points, tags }
}

async fn run(f: &Fixture, filters: serde_json::Value, sorts: serde_json::Value) -> Vec<String> {
    let v = views::create(&f.pool, f.collection, "V", "table", json!({})).await.unwrap();
    views::set_filters_and_sorts(&f.pool, v.id, filters, sorts).await.unwrap();
    let v = views::get(&f.pool, v.id).await.unwrap().unwrap();
    views::run(&f.pool, &v, f.ws, f.user, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.title)
        .collect()
}

#[tokio::test]
async fn an_unfiltered_view_returns_every_row() {
    let f = fixture().await;
    let titles = run(&f, json!([]), json!([])).await;
    assert_eq!(titles.len(), 3, "got {titles:?}");
}

/// **Filters run in SQL and actually filter.**
///
/// MECHANISM PROTECTED: the predicate compiler. Drop the `WHERE` it builds and
/// every case here returns all three rows.
#[tokio::test]
async fn each_operator_filters_correctly() {
    let f = fixture().await;

    let eq = run(&f, json!([{"property": f.status, "op": "eq", "value": "done"}]), json!([])).await;
    assert_eq!(eq, vec!["Ship auth"]);

    let neq = run(&f, json!([{"property": f.status, "op": "neq", "value": "done"}]), json!([])).await;
    assert_eq!(neq.len(), 2, "neq: {neq:?}");
    assert!(!neq.contains(&"Ship auth".to_string()));

    let gt = run(&f, json!([{"property": f.points, "op": "gt", "value": 2.0}]), json!([])).await;
    assert_eq!(gt.len(), 2, "numbers must compare numerically: {gt:?}");

    let lt = run(&f, json!([{"property": f.points, "op": "lt", "value": 2.0}]), json!([])).await;
    assert_eq!(lt, vec!["Write docs"]);

    // multi-select containment is array membership, not substring
    let has = run(&f, json!([{"property": f.tags, "op": "contains", "value": ["security"]}]), json!([])).await;
    assert_eq!(has, vec!["Ship auth"]);
}

/// `gt` on numbers must compare NUMERICALLY, not as text — otherwise 10 sorts
/// before 9 and every threshold filter is quietly wrong past single digits.
#[tokio::test]
async fn numbers_compare_numerically_not_lexically() {
    let f = fixture().await;
    let host: Uuid = sqlx::query_scalar("SELECT page_id FROM collections WHERE id = $1")
        .bind(f.collection).fetch_one(&f.pool).await.unwrap();
    let big: Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, parent_id, title) VALUES ($1, $2, 'Big') RETURNING id")
        .bind(f.ws).bind(host).fetch_one(&f.pool).await.unwrap();
    collections::set_value(&f.pool, big, f.points, json!(10.0)).await.unwrap();

    let gt = run(&f, json!([{"property": f.points, "op": "gt", "value": 9.0}]), json!([])).await;
    assert_eq!(gt, vec!["Big"], "10 > 9 numerically; as text '10' < '9'");
}

/// Two filters are ANDed, so adding one can only narrow the result.
#[tokio::test]
async fn multiple_filters_are_combined_with_and() {
    let f = fixture().await;
    let both = run(
        &f,
        json!([
            {"property": f.status, "op": "neq", "value": "done"},
            {"property": f.points, "op": "gt", "value": 2.0}
        ]),
        json!([]),
    ).await;
    assert_eq!(both, vec!["Table view"]);
}

/// Sorting is ORDER BY, and unset cells sort last in BOTH directions — "no
/// value" is not a small value.
#[tokio::test]
async fn sorts_order_rows_and_put_empty_cells_last() {
    let f = fixture().await;

    let asc = run(&f, json!([]), json!([{"property": f.points, "direction": "asc"}])).await;
    assert_eq!(asc, vec!["Write docs", "Table view", "Ship auth"]);

    let desc = run(&f, json!([]), json!([{"property": f.points, "direction": "desc"}])).await;
    assert_eq!(desc, vec!["Ship auth", "Table view", "Write docs"]);

    // A row with no value for the sort property.
    let host: Uuid = sqlx::query_scalar("SELECT page_id FROM collections WHERE id = $1")
        .bind(f.collection).fetch_one(&f.pool).await.unwrap();
    sqlx::query("INSERT INTO pages (workspace_id, parent_id, title) VALUES ($1, $2, 'No points')")
        .bind(f.ws).bind(host).execute(&f.pool).await.unwrap();

    for dir in ["asc", "desc"] {
        let out = run(&f, json!([]), json!([{"property": f.points, "direction": dir}])).await;
        assert_eq!(out.last().unwrap(), "No points", "{dir}: empty must sort last");
    }
}

/// `is_empty` / `is_not_empty` treat a JSON null as empty — a cleared cell is
/// empty, not "has the value null".
#[tokio::test]
async fn empty_means_unset_or_null() {
    let f = fixture().await;
    let host: Uuid = sqlx::query_scalar("SELECT page_id FROM collections WHERE id = $1")
        .bind(f.collection).fetch_one(&f.pool).await.unwrap();

    let unset: Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, parent_id, title) VALUES ($1, $2, 'Unset') RETURNING id")
        .bind(f.ws).bind(host).fetch_one(&f.pool).await.unwrap();
    let cleared: Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, parent_id, title) VALUES ($1, $2, 'Cleared') RETURNING id")
        .bind(f.ws).bind(host).fetch_one(&f.pool).await.unwrap();
    collections::set_value(&f.pool, cleared, f.status, json!(null)).await.unwrap();
    let _ = unset;

    let empty = run(&f, json!([{"property": f.status, "op": "is_empty"}]), json!([])).await;
    assert!(empty.contains(&"Unset".to_string()), "never set is empty: {empty:?}");
    assert!(empty.contains(&"Cleared".to_string()), "set to null is empty too: {empty:?}");
    assert!(!empty.contains(&"Ship auth".to_string()));

    let filled = run(&f, json!([{"property": f.status, "op": "is_not_empty"}]), json!([])).await;
    assert_eq!(filled.len(), 3);
}

/// **An unknown operator is an ERROR, not a silent pass-through.**
///
/// A typo'd operator that fell back to "no predicate" would widen a filter into
/// "match everything" — a view that quietly shows rows it was configured to
/// hide.
#[tokio::test]
async fn an_unknown_operator_is_refused() {
    let f = fixture().await;
    let v = views::create(&f.pool, f.collection, "V", "table", json!({})).await.unwrap();
    views::set_filters_and_sorts(
        &f.pool, v.id,
        json!([{"property": f.status, "op": "sounds_like", "value": "done"}]),
        json!([]),
    ).await.unwrap();
    let v = views::get(&f.pool, v.id).await.unwrap().unwrap();

    let err = views::run(&f.pool, &v, f.ws, f.user, 100).await.unwrap_err();
    assert!(matches!(err, ViewError::UnknownOperator(_)), "got {err}");
}

/// A filter naming a property from a DIFFERENT collection is refused — it is
/// how a crafted filter would otherwise reach another table's values.
#[tokio::test]
async fn a_filter_naming_a_foreign_property_is_refused() {
    let f = fixture().await;
    let other = fixture().await;

    let v = views::create(&f.pool, f.collection, "V", "table", json!({})).await.unwrap();
    views::set_filters_and_sorts(
        &f.pool, v.id,
        json!([{"property": other.status, "op": "eq", "value": "done"}]),
        json!([]),
    ).await.unwrap();
    let v = views::get(&f.pool, v.id).await.unwrap().unwrap();

    let err = views::run(&f.pool, &v, f.ws, f.user, 100).await.unwrap_err();
    assert!(matches!(err, ViewError::UnknownProperty), "got {err}");
}

/// A filter VALUE cannot become SQL. The value below is a classic injection
/// payload; it must be matched as a literal string and find nothing.
#[tokio::test]
async fn a_filter_value_is_bound_not_interpolated() {
    let f = fixture().await;
    let titles = run(
        &f,
        json!([{"property": f.status, "op": "eq", "value": "done'; DROP TABLE pages; --"}]),
        json!([]),
    ).await;
    assert!(titles.is_empty(), "the payload must match nothing: {titles:?}");

    // And the table is still there.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pages WHERE workspace_id = $1")
        .bind(f.ws).fetch_one(&f.pool).await.unwrap();
    assert!(n > 0, "pages must still exist");
}

/// A row the caller may not read does not appear in a table — a database row is
/// a page, so page permissions apply to it like anywhere else.
#[tokio::test]
async fn a_denied_row_does_not_appear_in_the_table() {
    let f = fixture().await;
    let all = run(&f, json!([]), json!([])).await;
    assert_eq!(all.len(), 3, "premise");

    let hidden: Uuid = sqlx::query_scalar("SELECT id FROM pages WHERE title = 'Ship auth' AND workspace_id = $1")
        .bind(f.ws).fetch_one(&f.pool).await.unwrap();
    noted_db::acl::set_access(&f.pool, hidden, f.user, "none").await.unwrap();

    let after = run(&f, json!([]), json!([])).await;
    assert_eq!(after.len(), 2, "the denied row must be gone: {after:?}");
    assert!(!after.contains(&"Ship auth".to_string()));
}

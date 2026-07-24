//! M3-3 — board and calendar views.
use noted_db::collections::{self, PropertyKind};
use noted_db::views;
use serde_json::json;
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let p = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&p).await.unwrap();
    p
}

struct F {
    pool: noted_db::PgPool,
    ws: Uuid,
    user: Uuid,
    coll: Uuid,
    host: Uuid,
    status: Uuid,
    due: Uuid,
}

async fn fixture() -> F {
    let pool = pool().await;
    let ws: Uuid = sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('board') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    let email = format!("b{}@example.com", Uuid::new_v4().simple());
    let user = noted_db::users::create(&pool, &email, "h", "B").await.unwrap().id;
    let host: Uuid = sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1,'Board') RETURNING id")
        .bind(ws).fetch_one(&pool).await.unwrap();
    let c = collections::create_collection(&pool, ws, host, "Board").await.unwrap();
    let status = collections::add_property(&pool, c.id, "Status", PropertyKind::Select,
        json!({"options": ["todo", "doing", "done"]}), 0).await.unwrap().id;
    let due = collections::add_property(&pool, c.id, "Due", PropertyKind::Date, json!({}), 1)
        .await.unwrap().id;
    F { pool, ws, user, coll: c.id, host, status, due }
}

async fn row(f: &F, title: &str, status: Option<&str>, due: Option<&str>) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, parent_id, title) VALUES ($1,$2,$3) RETURNING id")
        .bind(f.ws).bind(f.host).bind(title).fetch_one(&f.pool).await.unwrap();
    if let Some(s) = status {
        collections::set_value(&f.pool, id, f.status, json!(s)).await.unwrap();
    }
    if let Some(d) = due {
        collections::set_value(&f.pool, id, f.due, json!(d)).await.unwrap();
    }
    id
}

async fn board(f: &F) -> Vec<views::Group> {
    let v = views::create(&f.pool, f.coll, "Board", "board", json!({"group_by": f.status}))
        .await.unwrap();
    let v = views::get(&f.pool, v.id).await.unwrap().unwrap();
    views::run_board(&f.pool, &v, f.ws, f.user, 100).await.unwrap()
}

/// Columns follow the property's DECLARED option order, not the order rows
/// happened to arrive in — a board whose columns reshuffle is unusable.
#[tokio::test]
async fn board_columns_follow_the_declared_option_order() {
    let f = fixture().await;
    row(&f, "C", Some("done"), None).await;
    row(&f, "A", Some("todo"), None).await;
    row(&f, "B", Some("doing"), None).await;

    let groups = board(&f).await;
    let order: Vec<Option<String>> = groups.iter().map(|g| g.value.clone()).collect();
    assert_eq!(
        order,
        vec![Some("todo".into()), Some("doing".into()), Some("done".into()), None],
        "declared order, then the No value column last"
    );
}

/// **A row with no group value gets a COLUMN, not oblivion.**
///
/// MECHANISM PROTECTED: the `None` group. Drop it and the row vanishes from the
/// board while still existing in the table — which reads as data loss.
#[tokio::test]
async fn a_row_with_no_group_value_appears_in_its_own_column() {
    let f = fixture().await;
    row(&f, "Sorted", Some("todo"), None).await;
    row(&f, "Unsorted", None, None).await;

    let groups = board(&f).await;
    let empty = groups.iter().find(|g| g.value.is_none()).expect("a No value column must exist");
    let titles: Vec<&str> = empty.rows.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, vec!["Unsorted"]);

    let total: usize = groups.iter().map(|g| g.rows.len()).sum();
    assert_eq!(total, 2, "every row must be in exactly one column");
}

/// The empty column exists even when nothing is in it, so it is a visible drop
/// target rather than something that appears only once used.
#[tokio::test]
async fn the_no_value_column_exists_even_when_empty() {
    let f = fixture().await;
    row(&f, "Only", Some("todo"), None).await;
    let groups = board(&f).await;
    assert!(groups.iter().any(|g| g.value.is_none() && g.rows.is_empty()));
}

/// A value the property no longer declares still gets a column — the row exists
/// and someone has to be able to drag it out.
#[tokio::test]
async fn a_stale_option_value_still_gets_a_column() {
    let f = fixture().await;
    let r = row(&f, "Legacy", None, None).await;
    // Written directly: the property no longer offers this option.
    collections::set_value(&f.pool, r, f.status, json!("archived")).await.unwrap();

    let groups = board(&f).await;
    let stale = groups.iter().find(|g| g.value.as_deref() == Some("archived"));
    assert!(stale.is_some(), "a stale value must not vanish: {:?}",
        groups.iter().map(|g| &g.value).collect::<Vec<_>>());
    assert_eq!(stale.unwrap().rows.len(), 1);
}

/// **Moving a row between columns persists**, including into "No value".
#[tokio::test]
async fn moving_a_row_between_columns_persists() {
    let f = fixture().await;
    let r = row(&f, "Movable", Some("todo"), None).await;
    let v = views::create(&f.pool, f.coll, "Board", "board", json!({"group_by": f.status}))
        .await.unwrap();
    let v = views::get(&f.pool, v.id).await.unwrap().unwrap();

    views::move_to_group(&f.pool, &v, r, Some("doing")).await.unwrap();
    let groups = views::run_board(&f.pool, &v, f.ws, f.user, 100).await.unwrap();
    let doing = groups.iter().find(|g| g.value.as_deref() == Some("doing")).unwrap();
    assert_eq!(doing.rows.len(), 1, "the move must survive a reload");

    // Dragging into "No value" clears it.
    views::move_to_group(&f.pool, &v, r, None).await.unwrap();
    let groups = views::run_board(&f.pool, &v, f.ws, f.user, 100).await.unwrap();
    assert_eq!(groups.iter().find(|g| g.value.is_none()).unwrap().rows.len(), 1);
}

/// A calendar keys rows by day, and a timestamp and a bare date land in the
/// same cell.
#[tokio::test]
async fn a_calendar_buckets_by_day_regardless_of_time() {
    let f = fixture().await;
    row(&f, "Bare", None, Some("2026-07-22")).await;
    row(&f, "Stamped", None, Some("2026-07-22T14:30:00Z")).await;

    let v = views::create(&f.pool, f.coll, "Cal", "calendar", json!({"date_property": f.due}))
        .await.unwrap();
    let v = views::get(&f.pool, v.id).await.unwrap().unwrap();
    let (dated, _undated) = views::run_calendar(&f.pool, &v, f.ws, f.user, 100).await.unwrap();

    let days: Vec<&str> = dated.iter().map(|(d, _)| d.as_str()).collect();
    assert_eq!(days, vec!["2026-07-22", "2026-07-22"], "same cell");
}

/// **Undated rows are returned, not dropped.** A task with no due date is
/// exactly the task a user is hunting for; a calendar that omits it hides work.
#[tokio::test]
async fn a_calendar_returns_undated_rows_rather_than_hiding_them() {
    let f = fixture().await;
    row(&f, "Dated", None, Some("2026-07-22")).await;
    row(&f, "No due date", None, None).await;

    let v = views::create(&f.pool, f.coll, "Cal", "calendar", json!({"date_property": f.due}))
        .await.unwrap();
    let v = views::get(&f.pool, v.id).await.unwrap().unwrap();
    let (dated, undated) = views::run_calendar(&f.pool, &v, f.ws, f.user, 100).await.unwrap();

    assert_eq!(dated.len(), 1);
    assert_eq!(undated.len(), 1, "the undated row must come back");
    assert_eq!(undated[0].title, "No due date");
}

/// A board's filters still apply — grouping is a presentation of a filtered
/// set, not a way around the filter.
#[tokio::test]
async fn a_board_still_honours_its_filters() {
    let f = fixture().await;
    row(&f, "Keep", Some("todo"), None).await;
    row(&f, "Hide", Some("done"), None).await;

    let v = views::create(&f.pool, f.coll, "Board", "board", json!({"group_by": f.status}))
        .await.unwrap();
    views::set_filters_and_sorts(&f.pool, v.id,
        json!([{"property": f.status, "op": "neq", "value": "done"}]), json!([])).await.unwrap();
    let v = views::get(&f.pool, v.id).await.unwrap().unwrap();

    let groups = views::run_board(&f.pool, &v, f.ws, f.user, 100).await.unwrap();
    let total: usize = groups.iter().map(|g| g.rows.len()).sum();
    assert_eq!(total, 1, "the filtered-out row must not reappear via grouping");
}

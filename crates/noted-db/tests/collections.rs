//! M3-1 — collections, typed properties, and values.
use noted_db::collections::{self, PropertyError, PropertyKind};
use serde_json::json;
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

async fn workspace(pool: &noted_db::PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('coll-test') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn page(pool: &noted_db::PgPool, ws: Uuid, title: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
        .bind(ws)
        .bind(title)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn fixture(pool: &noted_db::PgPool) -> (Uuid, Uuid) {
    let ws = workspace(pool).await;
    let host = page(pool, ws, "Tasks").await;
    let c = collections::create_collection(pool, ws, host, "Tasks")
        .await
        .unwrap();
    (ws, c.id)
}

/// **A value of the wrong type is rejected at WRITE, not tolerated at read.**
///
/// MECHANISM PROTECTED: `validate`. Make it return `Ok(())` unconditionally and
/// every case here stores happily.
#[tokio::test]
async fn a_wrong_typed_value_is_refused_before_it_is_stored() {
    let pool = pool().await;
    let (ws, coll) = fixture(&pool).await;
    let row = page(&pool, ws, "Row").await;

    let cases: Vec<(PropertyKind, serde_json::Value, serde_json::Value)> = vec![
        (PropertyKind::Number, json!(42), json!("forty-two")),
        (PropertyKind::Checkbox, json!(true), json!("yes")),
        (PropertyKind::Text, json!("hello"), json!(7)),
        (PropertyKind::Date, json!("2026-07-22"), json!("last tuesday")),
        (
            PropertyKind::MultiSelect,
            json!(["a", "b"]),
            json!(["a", 3]),
        ),
        (PropertyKind::Url, json!("https://example.com"), json!(false)),
    ];

    for (i, (kind, good, bad)) in cases.into_iter().enumerate() {
        let p = collections::add_property(
            &pool,
            coll,
            &format!("prop{i}"),
            kind,
            json!({}),
            i as i32,
        )
        .await
        .unwrap();

        // The good value goes in, so a blanket rejection cannot pass this test.
        collections::set_value(&pool, row, p.id, good)
            .await
            .unwrap_or_else(|e| panic!("{kind:?} must accept its own type: {e}"));

        let err = collections::set_value(&pool, row, p.id, bad.clone())
            .await
            .expect_err(&format!("{kind:?} must reject {bad}"));
        assert!(
            matches!(err, PropertyError::WrongType { .. }),
            "{kind:?}: expected a type error, got {err}"
        );
    }
}

/// `null` clears a cell and is valid for every kind — an empty cell is a real
/// state, not a type violation.
#[tokio::test]
async fn null_is_accepted_for_every_kind() {
    let pool = pool().await;
    let (ws, coll) = fixture(&pool).await;
    let row = page(&pool, ws, "Row").await;

    for (i, kind) in [
        PropertyKind::Text,
        PropertyKind::Number,
        PropertyKind::Checkbox,
        PropertyKind::Date,
        PropertyKind::MultiSelect,
        PropertyKind::Select,
        PropertyKind::Url,
        PropertyKind::Relation,
    ]
    .into_iter()
    .enumerate()
    {
        let p = collections::add_property(&pool, coll, &format!("n{i}"), kind, json!({}), i as i32)
            .await
            .unwrap();
        collections::set_value(&pool, row, p.id, json!(null))
            .await
            .unwrap_or_else(|e| panic!("{kind:?} must accept null: {e}"));
    }
}

/// **Deleting a property removes its values** — by database cascade, so a crash
/// between two statements cannot orphan them and a future caller cannot forget.
///
/// MECHANISM PROTECTED: `ON DELETE CASCADE` on `page_properties.property_id`.
#[tokio::test]
async fn deleting_a_property_takes_its_values_with_it() {
    let pool = pool().await;
    let (ws, coll) = fixture(&pool).await;
    let row = page(&pool, ws, "Row").await;

    let doomed = collections::add_property(&pool, coll, "Status", PropertyKind::Text, json!({}), 0)
        .await
        .unwrap();
    let kept = collections::add_property(&pool, coll, "Owner", PropertyKind::Text, json!({}), 1)
        .await
        .unwrap();

    collections::set_value(&pool, row, doomed.id, json!("blocked"))
        .await
        .unwrap();
    collections::set_value(&pool, row, kept.id, json!("alice"))
        .await
        .unwrap();
    assert_eq!(collections::values_for_page(&pool, row).await.unwrap().len(), 2);

    collections::delete_property(&pool, doomed.id).await.unwrap();

    let left = collections::values_for_page(&pool, row).await.unwrap();
    assert_eq!(left.len(), 1, "the deleted property's value must be gone");
    assert_eq!(left[0].0, kept.id, "and the other one must survive");
}

/// Setting a value twice updates rather than duplicating or failing.
#[tokio::test]
async fn setting_a_value_twice_updates_it() {
    let pool = pool().await;
    let (ws, coll) = fixture(&pool).await;
    let row = page(&pool, ws, "Row").await;
    let p = collections::add_property(&pool, coll, "Status", PropertyKind::Text, json!({}), 0)
        .await
        .unwrap();

    collections::set_value(&pool, row, p.id, json!("todo")).await.unwrap();
    collections::set_value(&pool, row, p.id, json!("done")).await.unwrap();

    let values = collections::values_for_page(&pool, row).await.unwrap();
    assert_eq!(values.len(), 1, "one row per (page, property)");
    assert_eq!(values[0].1, json!("done"));
}

/// Properties come back in display order, and a collection cannot hold two
/// columns with the same name.
#[tokio::test]
async fn properties_are_ordered_and_uniquely_named() {
    let pool = pool().await;
    let (_ws, coll) = fixture(&pool).await;

    collections::add_property(&pool, coll, "Zeta", PropertyKind::Text, json!({}), 2).await.unwrap();
    collections::add_property(&pool, coll, "Alpha", PropertyKind::Text, json!({}), 0).await.unwrap();
    collections::add_property(&pool, coll, "Mid", PropertyKind::Text, json!({}), 1).await.unwrap();

    let names: Vec<String> = collections::properties(&pool, coll)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, vec!["Alpha", "Mid", "Zeta"], "position, not insertion order");

    let dup = collections::add_property(&pool, coll, "Alpha", PropertyKind::Text, json!({}), 9).await;
    assert!(dup.is_err(), "two columns cannot share a name");
}

/// Deleting the host page deletes the collection — which is what a user means
/// by deleting a database.
#[tokio::test]
async fn deleting_the_host_page_deletes_the_collection() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let host = page(&pool, ws, "Doomed database").await;
    let c = collections::create_collection(&pool, ws, host, "Tasks").await.unwrap();

    sqlx::query("DELETE FROM pages WHERE id = $1").bind(host).execute(&pool).await.unwrap();

    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM collections WHERE id = $1")
        .bind(c.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// A select's options live in `config`, so adding a property type never widens
/// the table.
#[tokio::test]
async fn kind_specific_config_round_trips() {
    let pool = pool().await;
    let (_ws, coll) = fixture(&pool).await;
    let p = collections::add_property(
        &pool,
        coll,
        "Priority",
        PropertyKind::Select,
        json!({"options": ["low", "high"]}),
        0,
    )
    .await
    .unwrap();

    let fetched = collections::properties(&pool, coll).await.unwrap();
    let found = fetched.iter().find(|f| f.id == p.id).unwrap();
    assert_eq!(found.config["options"][1], json!("high"));
    assert_eq!(found.kind, "select");
}

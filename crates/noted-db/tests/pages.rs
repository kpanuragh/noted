use noted_db::{docs, pages};

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('test') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    (pool, ws)
}

#[tokio::test]
async fn create_then_get_roundtrips() {
    let (pool, ws) = setup().await;
    let created = pages::create(&pool, ws, None, "Hello").await.unwrap();
    let fetched = pages::get(&pool, created.id).await.unwrap().unwrap();
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.title, "Hello");
    assert_eq!(fetched.parent_id, None);
}

#[tokio::test]
async fn children_returns_only_direct_children() {
    let (pool, ws) = setup().await;
    let root = pages::create(&pool, ws, None, "Root").await.unwrap();
    let child = pages::create(&pool, ws, Some(root.id), "Child")
        .await
        .unwrap();
    let _grandchild = pages::create(&pool, ws, Some(child.id), "Grandchild")
        .await
        .unwrap();

    let kids = pages::children(&pool, ws, Some(root.id)).await.unwrap();
    assert_eq!(
        kids.len(),
        1,
        "children() must not recurse into grandchildren"
    );
    assert_eq!(kids[0].id, child.id);
}

#[tokio::test]
async fn get_returns_none_for_unknown_id() {
    let (pool, _ws) = setup().await;
    let missing = pages::get(&pool, uuid::Uuid::new_v4()).await.unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn rename_updates_title_and_bumps_updated_at() {
    let (pool, ws) = setup().await;
    let p = pages::create(&pool, ws, None, "Before").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let renamed = pages::rename(&pool, p.id, "After").await.unwrap();
    assert!(renamed, "rename() must return true when a page was updated");
    let after = pages::get(&pool, p.id).await.unwrap().unwrap();
    assert_eq!(after.title, "After");
    assert!(
        after.updated_at > p.updated_at,
        "rename() must bump updated_at: before={}, after={}",
        p.updated_at,
        after.updated_at
    );
}

/// THE point of "recently edited": it must rank by when the content was last
/// touched, not by creation order. Driven through `docs::append` — the real
/// edit path — rather than by writing `updated_at` by hand, so this also pins
/// that the sync path and the dashboard agree.
#[tokio::test]
async fn recent_orders_by_true_edit_time_not_creation() {
    let (pool, ws) = setup().await;
    let older = pages::create(&pool, ws, None, "Created first")
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let newer = pages::create(&pool, ws, None, "Created second")
        .await
        .unwrap();

    // Now EDIT the older one, which by creation order would rank last.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    docs::append(&pool, older.id, b"an edit").await.unwrap();

    let hits = pages::recent(&pool, ws, 10).await.unwrap();
    let ids: Vec<_> = hits.iter().map(|p| p.id).collect();
    assert_eq!(
        ids,
        vec![older.id, newer.id],
        "the edited page must rank first even though it was created first"
    );
}

#[tokio::test]
async fn recent_excludes_archived_pages() {
    let (pool, ws) = setup().await;
    let live = pages::create(&pool, ws, None, "Live").await.unwrap();
    let gone = pages::create(&pool, ws, None, "Archived").await.unwrap();
    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(gone.id)
        .execute(&pool)
        .await
        .unwrap();

    let hits = pages::recent(&pool, ws, 10).await.unwrap();
    assert!(
        hits.iter().any(|p| p.id == live.id),
        "the live page must still appear"
    );
    assert!(
        !hits.iter().any(|p| p.id == gone.id),
        "an archived page must never appear in recently-edited"
    );
}

/// Tenancy. This is the defect class this project has produced most.
#[tokio::test]
async fn recent_is_scoped_to_the_workspace() {
    let (pool, ws_a) = setup().await;
    let (_, ws_b) = setup().await;
    let theirs = pages::create(&pool, ws_b, None, "Acquisition terms")
        .await
        .unwrap();
    // Make the other tenant's page the single most recently edited row in the
    // whole table, so an unscoped query would put it at position 0.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    docs::append(&pool, theirs.id, b"edit").await.unwrap();

    let hits = pages::recent(&pool, ws_a, 10).await.unwrap();
    assert!(
        !hits.iter().any(|p| p.id == theirs.id),
        "recent must never return another workspace's page"
    );
    assert!(
        hits.iter().all(|p| p.workspace_id == ws_a),
        "every row must belong to the requested workspace"
    );
}

#[tokio::test]
async fn recent_respects_the_limit_and_caps_it() {
    let (pool, ws) = setup().await;
    // MUST exceed MAX_RECENT_LIMIT, or the cap assertion below is vacuous: with
    // fewer live pages than the cap, an entirely uncapped query still returns
    // fewer than MAX_RECENT_LIMIT rows and the test passes while proving
    // nothing. Seeded in one statement — 60 round trips is needless.
    sqlx::query(
        "INSERT INTO pages (workspace_id, title)
         SELECT $1, 'p' || g::text FROM generate_series(1, $2) AS g",
    )
    .bind(ws)
    .bind(pages::MAX_RECENT_LIMIT as i32 + 10)
    .execute(&pool)
    .await
    .unwrap();

    let hits = pages::recent(&pool, ws, 2).await.unwrap();
    assert_eq!(hits.len(), 2, "a caller-supplied limit must be respected");

    // An uncapped limit is a trivial DoS: one request could ask for the whole
    // table. The cap lives in the repository so EVERY caller gets it, not just
    // the HTTP route.
    let hits = pages::recent(&pool, ws, 1_000_000).await.unwrap();
    assert!(
        hits.len() as i64 <= pages::MAX_RECENT_LIMIT,
        "recent must cap the limit at {}, got {}",
        pages::MAX_RECENT_LIMIT,
        hits.len()
    );

    // A nonsensical limit must not mean "no cap" (LIMIT -1 is an error in
    // Postgres, LIMIT 0 would silently return nothing).
    let hits = pages::recent(&pool, ws, 0).await.unwrap();
    assert!(
        !hits.is_empty(),
        "a zero/negative limit must not be honoured verbatim"
    );
}

/// EXPLAIN is the only accepted ground truth in this repository: three separate
/// "looks-indexed-but-isn't" bugs have shipped here behind queries that read as
/// though they used an index. A tiny table is cheaper to seq-scan honestly
/// regardless of query shape, so this seeds enough rows for the planner's
/// choice to mean something.
///
/// IGNORED BY DEFAULT: seeding 100k pages is slow, and a suite that slow stops
/// being run at all. Run it explicitly when touching `recent` or its index:
///   cargo test -p noted-db --test pages -- --ignored --nocapture
#[tokio::test]
#[ignore]
async fn recent_uses_the_workspace_updated_index() {
    let (pool, ws) = setup().await;

    sqlx::query(
        "INSERT INTO pages (workspace_id, title, updated_at)
         SELECT $1, 'Page ' || g::text, now() - (g || ' seconds')::interval
         FROM generate_series(1, 100000) AS g",
    )
    .bind(ws)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("ANALYZE pages").execute(&pool).await.unwrap();

    const EXPLAIN_RECENT: &str = "EXPLAIN (FORMAT TEXT)
         SELECT id, workspace_id, parent_id, title, created_at, updated_at
         FROM pages
         WHERE workspace_id = $1 AND archived_at IS NULL
         ORDER BY updated_at DESC, id DESC
         LIMIT $2";

    let rows: Vec<(String,)> = sqlx::query_as(EXPLAIN_RECENT)
        .bind(ws)
        .bind(10i64)
        .fetch_all(&pool)
        .await
        .unwrap();
    let after = rows
        .iter()
        .map(|r| r.0.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // COUNTERFACTUAL. `plan.contains(index_name)` on its own cannot distinguish
    // "the planner chose this index" from "any plan would have mentioned it" —
    // so drop the index and re-plan the identical query against identical data.
    // Inside a transaction that is always rolled back: Postgres DDL is
    // transactional, so the index is restored even if this test fails.
    // (DROP INDEX takes an ACCESS EXCLUSIVE lock on `pages` for the duration,
    // which is another reason this test is #[ignore]d rather than run inline
    // alongside the rest of the suite.)
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("DROP INDEX pages_workspace_updated_idx")
        .execute(&mut *tx)
        .await
        .unwrap();
    let rows: Vec<(String,)> = sqlx::query_as(EXPLAIN_RECENT)
        .bind(ws)
        .bind(10i64)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    let before = rows
        .iter()
        .map(|r| r.0.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    tx.rollback().await.unwrap();

    // Clean up BEFORE asserting: a failed assertion must not strand 100k pages
    // in the shared dev database. Deleting the workspace cascades to its pages.
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();

    eprintln!("=== WITHOUT the index ===\n{before}\n=== WITH the index ===\n{after}");

    assert!(
        before.contains("Sort") && !before.contains("pages_workspace_updated_idx"),
        "sanity check failed: the query without the index should sort explicitly; plan:\n{before}"
    );
    assert!(
        after.contains("pages_workspace_updated_idx"),
        "recent must use pages_workspace_updated_idx; plan:\n{after}"
    );
    assert!(
        !after.contains("Sort"),
        "the index must supply the ordering, not a post-hoc sort; plan:\n{after}"
    );
}

#[tokio::test]
async fn rename_unknown_page_returns_false() {
    let (pool, _ws) = setup().await;
    let renamed = pages::rename(&pool, uuid::Uuid::new_v4(), "Nope")
        .await
        .unwrap();
    assert!(
        !renamed,
        "rename() must return false when no page matches the id"
    );
}

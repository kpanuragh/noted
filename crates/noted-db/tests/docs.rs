use noted_db::docs;

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('docs-test') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    let page: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .unwrap();
    (pool, page)
}

#[tokio::test]
async fn append_then_load_preserves_order() {
    let (pool, page) = setup().await;
    docs::append(&pool, page, b"one").await.unwrap();
    docs::append(&pool, page, b"two").await.unwrap();
    docs::append(&pool, page, b"three").await.unwrap();

    let loaded = docs::load(&pool, page).await.unwrap();
    assert_eq!(
        loaded,
        vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
    );
}

#[tokio::test]
async fn compact_replaces_log_with_single_snapshot() {
    let (pool, page) = setup().await;
    for i in 0..5 {
        docs::append(&pool, page, format!("u{i}").as_bytes())
            .await
            .unwrap();
    }
    assert_eq!(docs::update_count(&pool, page).await.unwrap(), 5);

    docs::compact(&pool, page, b"snapshot").await.unwrap();

    assert_eq!(docs::update_count(&pool, page).await.unwrap(), 1);
    assert_eq!(
        docs::load(&pool, page).await.unwrap(),
        vec![b"snapshot".to_vec()]
    );
}

#[tokio::test]
async fn post_compaction_appends_sort_after_snapshot() {
    // This is an ORDERING test, not a concurrency test: every call below is
    // sequentially awaited (append -> compact -> append -> load), so it
    // exercises no concurrency at all. It only proves that an append issued
    // after compact() has returned sorts after the snapshot row. The actual
    // concurrency safety of compact() racing a live append is covered by
    // concurrent_appends_and_compact_keep_log_consistent below.
    let (pool, page) = setup().await;
    docs::append(&pool, page, b"before").await.unwrap();
    docs::compact(&pool, page, b"snap").await.unwrap();
    docs::append(&pool, page, b"after").await.unwrap();

    let loaded = docs::load(&pool, page).await.unwrap();
    assert_eq!(
        loaded,
        vec![b"snap".to_vec(), b"after".to_vec()],
        "post-compaction appends must sort after the snapshot"
    );
}

/// This test actually races append() against compact() using real concurrent
/// tasks on a multi-threaded runtime (unlike
/// post_compaction_appends_sort_after_snapshot, which is purely sequential).
///
/// What it proves: the log stays internally consistent under a genuine race
/// - it's never left empty, the snapshot survives, and doc_updates.seq stays
/// contiguous from 0 with no gaps or duplicates (a lost or double-claimed seq
/// would show up here).
///
/// What it does NOT prove: because this layer stores opaque bytes, it cannot
/// verify that a racing append's *content* was semantically folded into the
/// snapshot. That is the caller's concern (Task 8 builds the snapshot from
/// the in-memory doc, not from raw bytes chosen by this test).
///
/// ISOLATION — WHY `RACERS` IS 12 AND NOT 20. Every task here holds a pooled
/// connection for the whole of its transaction, and `noted_db::connect` builds a
/// 16-connection pool. At 20 racers plus the compaction this test wanted 21
/// connections from a pool of 16, so it had NEGATIVE headroom: it could only
/// finish by queueing, and any unrelated pressure on the shared development
/// database (another suite, a running server, a second agent) turned that
/// queueing into `PoolTimedOut` — a failure that looks like a product deadlock
/// and is not one. One such false alarm has already been investigated and
/// retracted on this branch. `RACERS + 1 < 16` removes the pool from the set of
/// things this test can fail on, and costs nothing: the invariant below needs a
/// genuine race, not a particular number of racers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_and_compact_keep_log_consistent() {
    const RACERS: usize = 12;

    let (pool, page) = setup().await;
    for i in 0..10 {
        docs::append(&pool, page, format!("seed{i}").as_bytes())
            .await
            .unwrap();
    }

    // Race RACERS appends against a compaction.
    let mut handles = Vec::new();
    for i in 0..RACERS {
        let p = pool.clone();
        handles.push(tokio::spawn(async move {
            docs::append(&p, page, format!("racer{i}").as_bytes()).await
        }));
    }
    let p = pool.clone();
    let compaction = tokio::spawn(async move { docs::compact(&p, page, b"snap").await });

    for h in handles {
        h.await
            .expect("append task panicked")
            .expect("append failed");
    }
    compaction
        .await
        .expect("compact task panicked")
        .expect("compact failed");

    // Invariants that must hold regardless of interleaving:
    let loaded = docs::load(&pool, page).await.unwrap();
    assert!(
        !loaded.is_empty(),
        "log must never be empty after compaction"
    );
    assert!(
        loaded.contains(&b"snap".to_vec()),
        "the snapshot must survive the race"
    );

    // seq must be contiguous from 0 with no gaps or duplicates - a lost or
    // double-claimed seq would show up here.
    let seqs: Vec<i64> =
        sqlx::query_scalar("SELECT seq FROM doc_updates WHERE page_id = $1 ORDER BY seq")
            .bind(page)
            .fetch_all(&pool)
            .await
            .unwrap();
    let expected: Vec<i64> = (0..seqs.len() as i64).collect();
    assert_eq!(seqs, expected, "doc_updates seq must be contiguous from 0");
}

/// `pages.updated_at` must track CONTENT edits, not just renames.
///
/// Before this, `rename` was the only writer of `updated_at`, while every real
/// edit flowed through the CRDT sync path into `docs::append` and never touched
/// it. Anything built on "recently edited" would have shown rename time: a user
/// could type all day and the page would not move.
///
/// STRICT `>`, and a real sleep first. `now()` is transaction-start time in
/// Postgres, so the create and the append genuinely get different values — but
/// a `>=` here would pass even if the fix were deleted entirely, which is the
/// exact trap this project has shipped before.
#[tokio::test]
async fn appending_a_doc_update_bumps_the_pages_updated_at() {
    let (pool, page) = setup().await;
    let before: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM pages WHERE id = $1")
            .bind(page)
            .fetch_one(&pool)
            .await
            .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    docs::append(&pool, page, b"an edit").await.unwrap();

    let after: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM pages WHERE id = $1")
            .bind(page)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        after > before,
        "docs::append must bump pages.updated_at: before={before}, after={after}"
    );
}

/// The bump must be in the SAME transaction as the append, so the two can never
/// disagree.
///
/// WHERE THE FAILURE IS RIGGED IS THE ENTIRE TEST. An earlier version of this
/// test collided `doc_seq` so the INSERT into `doc_updates` failed — which is
/// BEFORE the bump, so `?` returned early and the bump never ran at all. It
/// therefore passed whether the bump was in the transaction or on the pool, and
/// proved nothing about the property it is named for.
///
/// So the failure has to happen strictly AFTER the bump statement has succeeded,
/// and the only such point is COMMIT. A `DEFERRABLE INITIALLY DEFERRED`
/// constraint trigger is the one portable way to put a failure there: its body
/// runs when the transaction commits, so `docs::append` gets all the way through
/// the `UPDATE pages` and then the commit is refused. If the bump has escaped to
/// its own transaction it has ALREADY committed by then and `updated_at` shows
/// the edit that the log does not contain — which is exactly the disagreement
/// the doc comment on `docs::append` claims is unrepresentable.
///
/// The trigger is scoped by a `WHEN` clause to this test's page, so it cannot
/// affect any other row, and it is torn down before the assertions run so a
/// failing assertion still leaves the shared dev database clean.
#[tokio::test]
async fn a_failed_append_does_not_bump_updated_at() {
    let (pool, page) = setup().await;
    docs::append(&pool, page, b"first").await.unwrap();

    // SAFETY (AssertSqlSafe): every interpolated value below is a `Uuid`
    // rendered by its own `Display`, never user input. DDL cannot take bind
    // parameters, so interpolation is the only option here.
    let trig = format!("noted_test_fail_at_commit_{}", page.simple());
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION {trig}() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'rigged commit-time failure'; END $$"
    )))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE CONSTRAINT TRIGGER {trig} AFTER INSERT ON doc_updates \
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.page_id = '{page}') \
         EXECUTE FUNCTION {trig}()"
    )))
    .execute(&pool)
    .await
    .unwrap();

    let before: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM pages WHERE id = $1")
            .bind(page)
            .fetch_one(&pool)
            .await
            .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let err = docs::append(&pool, page, b"fails at commit").await;

    // Torn down BEFORE the assertions: a failure below must not leave DDL debris
    // behind in a database other tests share.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER {trig} ON doc_updates"
    )))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP FUNCTION {trig}()")))
        .execute(&pool)
        .await
        .unwrap();

    let err = err.expect_err("the rigged append must fail");
    assert!(
        err.to_string().contains("rigged commit-time failure"),
        "the failure must be the deferred trigger firing at COMMIT — i.e. after the bump ran, \
         not before it. Got: {err}"
    );
    assert!(
        docs::load(&pool, page).await.unwrap() == vec![b"first".to_vec()],
        "the rigged append must have rolled its doc_updates row back"
    );

    let after: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM pages WHERE id = $1")
            .bind(page)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        after, before,
        "an append whose COMMIT failed must not leave pages.updated_at claiming an edit that is \
         not in the log — which is only true if the bump rolled back with it"
    );
}

#[tokio::test]
async fn load_for_page_with_no_updates_is_empty() {
    let (pool, page) = setup().await;
    assert!(docs::load(&pool, page).await.unwrap().is_empty());
}

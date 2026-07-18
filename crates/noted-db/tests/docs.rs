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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_and_compact_keep_log_consistent() {
    let (pool, page) = setup().await;
    for i in 0..10 {
        docs::append(&pool, page, format!("seed{i}").as_bytes())
            .await
            .unwrap();
    }

    // Race 20 appends against a compaction.
    let mut handles = Vec::new();
    for i in 0..20 {
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
/// disagree. Proven from the outside: a failed append (duplicate `seq`, forced
/// by hand here) must leave `updated_at` untouched, which is only true if the
/// UPDATE rolls back with it.
#[tokio::test]
async fn a_failed_append_does_not_bump_updated_at() {
    let (pool, page) = setup().await;
    docs::append(&pool, page, b"first").await.unwrap();

    // Rewind the sequence so the next append collides with seq 0 and the
    // INSERT into doc_updates fails after the doc_seq claim has been made.
    sqlx::query("UPDATE doc_seq SET next = 0 WHERE page_id = $1")
        .bind(page)
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

    let err = docs::append(&pool, page, b"collides").await;
    assert!(err.is_err(), "the rigged append must fail");

    let after: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM pages WHERE id = $1")
            .bind(page)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        after, before,
        "an append that failed must not leave pages.updated_at claiming an edit"
    );
}

#[tokio::test]
async fn load_for_page_with_no_updates_is_empty() {
    let (pool, page) = setup().await;
    assert!(docs::load(&pool, page).await.unwrap().is_empty());
}

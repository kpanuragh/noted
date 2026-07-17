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

#[tokio::test]
async fn load_for_page_with_no_updates_is_empty() {
    let (pool, page) = setup().await;
    assert!(docs::load(&pool, page).await.unwrap().is_empty());
}

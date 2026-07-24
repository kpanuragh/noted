//! The upgrade path: an instance that predates the embedding pipeline.
//!
//! `rechunk_page` is only called when a page is EDITED (the debounced projection)
//! or explicitly reprojected. An M1a instance therefore has a fully populated
//! `blocks` table and a completely empty `page_chunks` — so a CLI that only drains
//! the queue finds nothing, reports "done", and exits 0 having indexed a whole
//! vault's worth of nothing.
//!
//! These tests pin the sequence the `noted-index` binary runs: materialise chunks
//! for every page FIRST, then drain.

/// Simulates an M1a instance: blocks written straight to the table, exactly as
/// M1a's block projection left them, with NOTHING in `page_chunks`.
///
/// This deliberately does NOT call `rechunk_page` — calling it here would
/// materialise the chunks the code under test is supposed to materialise, and the
/// test would pass against a CLI that never backfills anything.
async fn m1a_instance(pages: usize) -> (noted_db::PgPool, Vec<uuid::Uuid>, String) {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();

    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('backfill-test') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();

    // A marker unique to this run: the database is shared with every other test in
    // the suite, so assertions must key off THIS corpus rather than global counts.
    let marker = format!("backfillmarker{}", uuid::Uuid::new_v4().simple());

    let mut page_ids = Vec::new();
    for p in 0..pages {
        let page: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id",
        )
        .bind(ws)
        .fetch_one(&pool)
        .await
        .unwrap();

        let body = std::iter::repeat("word")
            .take(100)
            .collect::<Vec<_>>()
            .join(" ");
        let text = format!("{marker} page {p} {body}");
        sqlx::query(
            "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
             VALUES ($1, 0, 'paragraph', $2, md5($2))",
        )
        .bind(page)
        .bind(&text)
        .execute(&pool)
        .await
        .unwrap();

        page_ids.push(page);
    }
    (pool, page_ids, marker)
}

/// THE test for the upgrade path. Mirrors the CLI's sequence exactly:
/// `all_page_ids` -> `rechunk_page` for each -> the queue has work.
///
/// If the CLI's rechunk loop is removed, `page_chunks` stays empty, `pending()`
/// returns nothing, and this fails — which is the whole point of it.
#[tokio::test]
async fn backfills_a_corpus_that_predates_the_pipeline() {
    let (pool, page_ids, marker) = m1a_instance(3).await;

    // Precondition: this really is an M1a instance. The blocks exist...
    let blocks: i64 = sqlx::query_scalar("SELECT count(*) FROM blocks WHERE text LIKE $1")
        .bind(format!("{marker}%"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        blocks, 3,
        "setup must have written blocks directly, as M1a would"
    );

    // ...and nothing has been chunked, so the queue is empty and a drain-only CLI
    // would report success having done nothing.
    let unchunked = noted_db::chunks::pending(&pool, "backfill-model", None, 1000)
        .await
        .unwrap();
    assert!(
        !unchunked.iter().any(|c| c.text.contains(&marker)),
        "precondition: an M1a corpus must have NO chunks queued before the backfill runs"
    );

    // The sequence the CLI runs.
    let all = noted_db::pages::all_page_ids(&pool).await.unwrap();
    for page_id in &page_ids {
        assert!(
            all.contains(page_id),
            "all_page_ids must return every live page"
        );
    }
    let mut chunked = 0usize;
    for page_id in &all {
        chunked += noted_index::materialize::rechunk_page(&pool, *page_id)
            .await
            .unwrap();
    }
    assert!(
        chunked > 0,
        "materialising an existing corpus must produce chunks"
    );

    // The payoff: the queue now has this corpus's chunks in it.
    let pending = noted_db::chunks::pending(&pool, "backfill-model", None, 1000)
        .await
        .unwrap();
    let mine: Vec<_> = pending
        .iter()
        .filter(|c| c.text.contains(&marker))
        .collect();
    assert!(
        !mine.is_empty(),
        "after materialising, a pre-existing corpus must appear in the work queue; \
         an empty queue here is the silent 'indexed nothing' failure"
    );
    assert_eq!(
        mine.len(),
        3,
        "every page in the corpus must contribute a chunk, got {}",
        mine.len()
    );
}

/// `all_page_ids` feeds the backfill, so an archived page slipping in would
/// resurrect deleted content into the index.
#[tokio::test]
async fn all_page_ids_excludes_archived_pages() {
    let (pool, page_ids, _marker) = m1a_instance(2).await;
    let archived = page_ids[0];
    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(archived)
        .execute(&pool)
        .await
        .unwrap();

    let all = noted_db::pages::all_page_ids(&pool).await.unwrap();
    assert!(
        !all.contains(&archived),
        "an archived page must not be backfilled"
    );
    assert!(
        all.contains(&page_ids[1]),
        "a live page must still be backfilled"
    );
}

use noted_db::search;

/// A user for the ACL filter. Every page in these fixtures is readable by
/// default (no overrides), so the filter is a pass-through here — `tests/acl.rs`
/// is where the denial behaviour is proven.
async fn acl_user(pool: &noted_db::PgPool) -> uuid::Uuid {
    let email = format!("qf{}@example.com", uuid::Uuid::new_v4().simple());
    noted_db::users::create(pool, &email, "hash", "QF")
        .await
        .unwrap()
        .id
}

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('qf-test') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    (pool, ws)
}

async fn page(pool: &noted_db::PgPool, ws: uuid::Uuid, title: &str) -> uuid::Uuid {
    sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
        .bind(ws).bind(title).fetch_one(pool).await.unwrap()
}

/// The whole point of quick find: an exact title match wins. If this ranks
/// second, the product reads as broken.
#[tokio::test]
async fn an_exact_title_match_ranks_first() {
    let (pool, ws) = setup().await;
    let _ = page(&pool, ws, "Postgres tuning notes from last quarter").await;
    let exact = page(&pool, ws, "Quarterly Report").await;
    let _ = page(&pool, ws, "Quarterly Report follow-up actions").await;

    let hits = search::quick_find(&pool, ws, acl_user(&pool).await, "Quarterly Report", 10).await.unwrap();
    assert!(!hits.is_empty(), "quick find must return the matching pages");
    assert_eq!(hits[0].page_id, exact, "the exact title match must rank first, got {:?}", hits[0].title);
}

#[tokio::test]
async fn a_prefix_match_is_found() {
    let (pool, ws) = setup().await;
    let p = page(&pool, ws, "Deployment runbook").await;
    let hits = search::quick_find(&pool, ws, acl_user(&pool).await, "Deploy", 10).await.unwrap();
    assert!(hits.iter().any(|h| h.page_id == p), "a prefix of the title must match");
}

/// Quick find must not leak titles across tenants.
#[tokio::test]
async fn quick_find_is_scoped_to_the_workspace() {
    let (pool, ws_a) = setup().await;
    let (_, ws_b) = setup().await;
    let secret = page(&pool, ws_b, "Acquisition terms").await;

    let hits = search::quick_find(&pool, ws_a, acl_user(&pool).await, "Acquisition", 10).await.unwrap();
    assert!(
        !hits.iter().any(|h| h.page_id == secret),
        "quick find must never return a page from another workspace"
    );
}

#[tokio::test]
async fn archived_pages_are_not_found() {
    let (pool, ws) = setup().await;
    let p = page(&pool, ws, "Deleted thing").await;
    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(p).execute(&pool).await.unwrap();
    let hits = search::quick_find(&pool, ws, acl_user(&pool).await, "Deleted", 10).await.unwrap();
    assert!(!hits.iter().any(|h| h.page_id == p), "archived pages must not appear");
}

#[tokio::test]
async fn an_empty_query_returns_nothing_rather_than_everything() {
    let (pool, ws) = setup().await;
    let _ = page(&pool, ws, "Something").await;
    let hits = search::quick_find(&pool, ws, acl_user(&pool).await, "   ", 10).await.unwrap();
    assert!(hits.is_empty(), "a blank query must not dump the whole workspace");
}

/// `%` and `_` are LIKE/ILIKE metacharacters. If the user's query is bound
/// straight into an ILIKE pattern without escaping, "50%" becomes a wildcard
/// that matches everything instead of a literal percent sign.
#[tokio::test]
async fn wildcards_in_the_query_are_literal() {
    let (pool, ws) = setup().await;
    let done = page(&pool, ws, "50% complete").await;
    let _ = page(&pool, ws, "Nothing to do with numbers").await;

    let hits = search::quick_find(&pool, ws, acl_user(&pool).await, "50%", 10).await.unwrap();
    assert!(
        hits.iter().any(|h| h.page_id == done),
        "a literal '50%' in the query must still find the page titled '50% complete'"
    );

    let hits = search::quick_find(&pool, ws, acl_user(&pool).await, "%", 10).await.unwrap();
    assert!(
        hits.len() < 2,
        "a bare '%' must not act as a wildcard matching every page in the workspace, got {:?}",
        hits.iter().map(|h| &h.title).collect::<Vec<_>>()
    );
}

/// This is the whole point of the fix: prove the trigram index
/// (`pages_title_trgm_idx`, built on `title` in M1a) is actually chosen by
/// the planner for the query shape `quick_find` runs, not just that the
/// query *looks* like it should use it.
///
/// A tiny table is cheaper to seq-scan regardless of query shape, so the
/// planner would honestly pick a seq scan either way and the test would
/// prove nothing. 2,000-2,500 rows in a single fresh workspace was tried
/// first and was NOT enough here: with only one workspace_id value in play,
/// the planner leans on `pages_workspace_parent_idx` alone (its selectivity
/// estimate for a brand-new UUID is inaccurate) and applies the title
/// predicate as a post-filter instead of reaching for the trigram index.
/// At 100k rows — the scale explicitly called out in the review as where
/// this bites in production — the planner's cost model shifts and it
/// genuinely combines both indexes via a BitmapAnd. So we insert 100k pages,
/// `ANALYZE` so the planner has fresh statistics, then EXPLAIN both the old
/// (broken) query shape and the new (fixed) one against the same data, to
/// show the before/after.
/// IGNORED BY DEFAULT: seeding 100k pages takes minutes, and a suite that slow
/// stops being run at all — which costs more than this test catches. Run it
/// explicitly when touching the query shape or its indexes:
///   cargo test -p noted-db --test quickfind -- --ignored --nocapture
#[tokio::test]
#[ignore]
async fn quick_find_uses_the_trigram_index() {
    let (pool, ws) = setup().await;

    sqlx::query(
        "INSERT INTO pages (workspace_id, title)
         SELECT $1,
                'Report on topic ' || (g % 137)::text || ' revision ' || g::text
         FROM generate_series(1, 100000) AS g",
    )
    .bind(ws)
    .execute(&pool)
    .await
    .unwrap();
    let _ = page(&pool, ws, "Quarterly Planning Deck").await;

    sqlx::query("ANALYZE pages").execute(&pool).await.unwrap();

    // --- BEFORE: the old, broken query shape (lower(title) LIKE / bare similarity()) ---
    let before_rows: Vec<(String,)> = sqlx::query_as(
        "EXPLAIN (FORMAT TEXT)
         SELECT id AS page_id, title,
                CASE
                  WHEN lower(title) = lower($2)            THEN 1.0
                  WHEN lower(title) LIKE lower($2) || '%'  THEN 0.9
                  ELSE similarity(title, $2) * 0.8
                END::real AS rank
         FROM pages
         WHERE workspace_id = $1
           AND archived_at IS NULL
           AND (lower(title) LIKE '%' || lower($2) || '%' OR similarity(title, $2) > 0.15)
         ORDER BY rank DESC, title
         LIMIT $3",
    )
    .bind(ws)
    .bind("Planning")
    .bind(10i64)
    .fetch_all(&pool)
    .await
    .unwrap();
    let before_plan = before_rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>().join("\n");

    // --- AFTER: the fixed query shape, exactly as `quick_find` now runs it ---
    let after_rows: Vec<(String,)> = sqlx::query_as(
        "EXPLAIN (FORMAT TEXT)
         SELECT id AS page_id, title,
                CASE
                  WHEN lower(title) = lower($2)   THEN 1.0
                  WHEN title ILIKE $3 || '%'      THEN 0.9
                  ELSE similarity(title, $2) * 0.8
                END::real AS rank
         FROM pages
         WHERE workspace_id = $1
           AND archived_at IS NULL
           AND (title ILIKE '%' || $3 || '%' OR title % $2)
         ORDER BY rank DESC, title
         LIMIT $4",
    )
    .bind(ws)
    .bind("Planning")
    .bind("Planning")
    .bind(10i64)
    .fetch_all(&pool)
    .await
    .unwrap();
    let after_plan = after_rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>().join("\n");

    eprintln!("=== BEFORE (old query shape) plan ===\n{before_plan}");
    eprintln!("=== AFTER (fixed query shape) plan ===\n{after_plan}");

    assert!(
        !before_plan.contains("pages_title_trgm_idx"),
        "sanity check failed: the OLD query shape unexpectedly used the trigram index; plan:\n{before_plan}"
    );
    assert!(
        after_plan.contains("pages_title_trgm_idx"),
        "the FIXED query must use pages_title_trgm_idx (index not chosen); plan:\n{after_plan}"
    );
}

use noted_db::search;

const M: &str = "test-model";

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('hy-test') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    (pool, ws)
}

/// A page with real text (so FTS can find it) and a controlled embedding (so
/// the vector arm can).
async fn page_with(
    pool: &noted_db::PgPool, ws: uuid::Uuid, title: &str, text: &str, axis: usize,
) -> uuid::Uuid {
    let page: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
        .bind(ws).bind(title).fetch_one(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
         VALUES ($1, 0, 'paragraph', $2, md5($2))")
        .bind(page).bind(text).execute(pool).await.unwrap();
    let hash = format!("hy-{}", uuid::Uuid::new_v4());
    noted_db::chunks::upsert(pool, &[(hash.clone(), text.to_string(), 10)]).await.unwrap();
    noted_db::chunks::set_page_chunks(pool, page, &[hash.clone()]).await.unwrap();
    let mut v = vec![0.0f32; 768];
    v[axis] = 1.0;
    noted_db::chunks::store_embedding(pool, &hash, M, &v).await.unwrap();
    page
}

fn vec_at(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; 768];
    v[axis] = 1.0;
    v
}

/// The lexical arm's job: an exact rare term must be found even when the query
/// vector points nowhere near it. This is what pure vector search gets wrong.
#[tokio::test]
async fn an_exact_rare_term_is_found_by_the_lexical_arm() {
    let (pool, ws) = setup().await;
    let p = page_with(&pool, ws, "Errors", "the deploy failed with error ECONNREFUSED today", 0).await;

    // Query vector deliberately points at an unrelated axis: only FTS can win.
    let hits = search::hybrid(&pool, ws, "ECONNREFUSED", &vec_at(700), M, 10).await.unwrap();
    assert!(
        hits.iter().any(|h| h.page_id == p),
        "an exact rare term must be found via the lexical arm even when the vector arm misses"
    );
}

/// The vector arm's job: find a page whose words do not match the query at all.
#[tokio::test]
async fn a_semantic_match_is_found_by_the_vector_arm() {
    let (pool, ws) = setup().await;
    let p = page_with(&pool, ws, "Planner", "the query planner keeps choosing a seq scan", 3).await;

    // Query words appear nowhere in the text; only the vector arm can find it.
    let hits = search::hybrid(&pool, ws, "zzzznomatchzzz", &vec_at(3), M, 10).await.unwrap();
    assert!(
        hits.iter().any(|h| h.page_id == p),
        "a semantic match must be found via the vector arm when no words match"
    );
}

/// RRF's whole purpose: a page both arms agree on beats a page only one found.
///
/// NOTE the deliberate term repetition in `both`'s text. Without it, both pages
/// contain "postgres" exactly once, `ts_rank_cd` TIES, and ROW_NUMBER breaks the
/// tie arbitrarily — which makes the RRF scores come out as 1/62 + 1/61 versus
/// 1/61 + 1/62, an exact tie, and the assertion passes only ~half the time.
/// Repeating the term makes the lexical rank deterministic so this test measures
/// fusion rather than coin-flips.
#[tokio::test]
async fn a_page_found_by_both_arms_outranks_one_found_by_only_one() {
    let (pool, ws) = setup().await;
    let both = page_with(&pool, ws, "Both", "postgres postgres tuning advice", 5).await;
    let lex_only = page_with(&pool, ws, "LexOnly", "postgres unrelated topic", 600).await;

    let hits = search::hybrid(&pool, ws, "postgres", &vec_at(5), M, 10).await.unwrap();
    assert!(hits.len() >= 2, "both pages match lexically and must both be returned");
    assert_eq!(
        hits[0].page_id, both,
        "the page both arms agree on must rank first — that is what RRF is for"
    );

    let both_score = hits.iter().find(|h| h.page_id == both).unwrap().score;
    let lex_score = hits.iter().find(|h| h.page_id == lex_only).unwrap().score;
    assert!(
        both_score > lex_score,
        "fused score must be strictly greater, not tied: both={both_score} lex_only={lex_score}"
    );
}

#[tokio::test]
async fn hybrid_is_scoped_to_the_workspace() {
    let (pool, ws_a) = setup().await;
    let (_, ws_b) = setup().await;
    let secret = page_with(&pool, ws_b, "Secret", "acquisition terms and price", 9).await;

    let hits = search::hybrid(&pool, ws_a, "acquisition", &vec_at(9), M, 10).await.unwrap();
    assert!(
        !hits.iter().any(|h| h.page_id == secret),
        "search must never cross a workspace boundary"
    );
}

#[tokio::test]
async fn an_empty_query_returns_nothing() {
    let (pool, ws) = setup().await;
    let _ = page_with(&pool, ws, "Thing", "some text here", 0).await;
    let hits = search::hybrid(&pool, ws, "  ", &vec_at(0), M, 10).await.unwrap();
    assert!(hits.is_empty(), "a blank query must not dump the workspace");
}

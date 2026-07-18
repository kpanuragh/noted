use noted_db::search;

/// A fresh, never-before-used model_id per test — see the identical helper
/// (and rationale) in `related.rs`. `embeddings_hnsw_idx` is an approximate
/// index whose recall degrades as the vector count under a single `model_id`
/// grows; a unique `model_id` per test keeps each test's vector space small
/// enough that ANN search is exact for it.
fn unique_model() -> String {
    format!("test-model-{}", uuid::Uuid::new_v4())
}

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
    pool: &noted_db::PgPool, ws: uuid::Uuid, title: &str, text: &str, axis: usize, model: &str,
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
    noted_db::chunks::store_embedding(pool, &hash, model, &v).await.unwrap();
    page
}

fn vec_at(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; 768];
    v[axis] = 1.0;
    v
}

/// A page with text but deliberately NO embedding at all (under any model). The
/// lexical arm reads straight from `blocks`, so this page is reachable ONLY via
/// FTS — it can never appear in the vector arm's candidate set, no matter how
/// small the workspace is. Used to build decoys that are genuinely single-arm
/// (lexical-only), as opposed to `page_with`'s decoys which are always ALSO a
/// (trivial) vector-arm candidate once embedded, since a small test workspace
/// never exceeds the vector arm's top-100 cutoff.
async fn page_lexical_only(
    pool: &noted_db::PgPool, ws: uuid::Uuid, title: &str, text: &str,
) -> uuid::Uuid {
    let page: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
        .bind(ws).bind(title).fetch_one(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
         VALUES ($1, 0, 'paragraph', $2, md5($2))")
        .bind(page).bind(text).execute(pool).await.unwrap();
    page
}

/// The lexical arm's job: an exact rare term must be found even when the query
/// vector points nowhere near it. This is what pure vector search gets wrong.
///
/// With only 1-2 fixture pages, the vector arm's top-100 candidate set trivially
/// contains every page in the workspace, so the target would surface even if the
/// lexical arm were deleted entirely — the test wouldn't actually prove lexical
/// isolation. To make it a real test, we add five decoy pages whose embeddings
/// sit on/near the query's axis (so they legitimately beat the target in the
/// vector ranking) and that do NOT contain the rare term. If the lexical arm
/// were removed, the target — far from `q_vec` and buried behind five closer
/// decoys — would rank last or vanish entirely.
#[tokio::test]
async fn an_exact_rare_term_is_found_by_the_lexical_arm() {
    let (pool, ws) = setup().await;
    let model = unique_model();
    let target =
        page_with(&pool, ws, "Errors", "the deploy failed with error ECONNREFUSED today", 0, &model).await;

    // Decoys: embeddings on axis 700, EXACTLY matching q_vec (distance 0), so
    // they are strictly closer to q_vec than the target (axis 0, distance 1,
    // since these are orthonormal one-hot vectors). No rare term in their text.
    let mut decoys = Vec::new();
    for i in 0..5 {
        let title = format!("Decoy{i}");
        let text = format!("some unrelated notes about topic number {i}");
        decoys.push(page_with(&pool, ws, &title, &text, 700, &model).await);
    }

    // Query vector deliberately points at the decoys' axis: only FTS can win on rank.
    let hits = search::hybrid(&pool, ws, "ECONNREFUSED", &vec_at(700), &model, 10).await.unwrap();

    let target_pos = hits.iter().position(|h| h.page_id == target).expect(
        "an exact rare term must be found via the lexical arm even when the vector arm buries it behind closer decoys",
    );
    for decoy in &decoys {
        if let Some(decoy_pos) = hits.iter().position(|h| h.page_id == *decoy) {
            assert!(
                target_pos < decoy_pos,
                "the lexically-matched target must outrank vector-only decoys"
            );
        }
    }
}

/// The vector arm's job: find a page whose words do not match the query at all.
///
/// Mirrors the lexical-arm strengthening above, adjusted for an asymmetry in
/// RRF: a page found by BOTH arms always outranks one found by only one arm
/// (that is what `a_page_found_by_both_arms_outranks_one_found_by_only_one`
/// exists to prove). `page_with` decoys carrying a real embedding under the
/// queried model would land in the vector arm's top-100 candidate set too — a small test
/// workspace never exceeds that cutoff — making them dual-arm hits that would
/// always out-score a single-arm target by construction, regardless of how far
/// their embedding sits from `q_vec`. That is correct RRF behaviour, not a bug,
/// so asserting the target beats such decoys would be fighting the
/// implementation rather than testing it.
///
/// So decoys here are built with `page_lexical_only`: they contain the query
/// words but have NO embedding under any model, so they can never enter the
/// vector arm's candidate set — genuinely single-arm (lexical only), mirroring
/// the target's genuinely single-arm (vector only) status. A `distractor` page
/// with the term repeated several times is added purely to take lexical rank 1
/// away from the decoys — without it, a decoy could tie the target's RRF score
/// exactly (both rank 1 in their own single arm) and the comparison would be
/// nondeterministic.
#[tokio::test]
async fn a_semantic_match_is_found_by_the_vector_arm() {
    let (pool, ws) = setup().await;
    let model = unique_model();
    let target =
        page_with(&pool, ws, "Planner", "the query planner keeps choosing a seq scan", 3, &model).await;

    let _distractor = page_lexical_only(
        &pool, ws, "Distractor",
        "zzzznomatchzzz zzzznomatchzzz zzzznomatchzzz zzzznomatchzzz zzzznomatchzzz outranks decoys lexically",
    ).await;

    // Decoys: contain the query words (single mention, so they rank below the
    // distractor), no embedding under any model, so vector-arm-invisible.
    let mut decoys = Vec::new();
    for i in 0..5 {
        let title = format!("Decoy{i}");
        let text = format!("zzzznomatchzzz appears right here in decoy {i}");
        decoys.push(page_lexical_only(&pool, ws, &title, &text).await);
    }

    // Query words appear nowhere in the target's text; only the vector arm can find it.
    let hits = search::hybrid(&pool, ws, "zzzznomatchzzz", &vec_at(3), &model, 10).await.unwrap();

    let target_pos = hits.iter().position(|h| h.page_id == target).expect(
        "a semantic match must be found via the vector arm even when lexical decoys share the query words",
    );
    for decoy in &decoys {
        let decoy_pos = hits
            .iter()
            .position(|h| h.page_id == *decoy)
            .expect("lexical decoys should be found via FTS");
        assert!(
            target_pos <= decoy_pos,
            "the true semantic match must rank at or above lexical-only decoys"
        );
    }
}

/// The vector arm filters `e.model_id = $4`. Nothing about the query SHAPE
/// would fail if that filter were dropped — it would just silently compare
/// cosine distances across incompatible embedding spaces. This test creates
/// two pages with an IDENTICAL embedding on the same axis, one under the
/// queried model and one under a different model, and asserts only the
/// queried-model page is returned.
/// Both pages deliberately omit the query term so only the vector arm can find
/// either of them — a passing lexical arm cannot mask a broken model filter.
#[tokio::test]
async fn hybrid_vector_arm_ignores_other_models() {
    let (pool, ws) = setup().await;
    let model = unique_model();

    let target = page_with(&pool, ws, "Target", "notes without the query term", 5, &model).await;

    // Decoy: identical embedding axis, but stored under a DIFFERENT model.
    let decoy: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
        .bind(ws).bind("Decoy").fetch_one(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
         VALUES ($1, 0, 'paragraph', $2, md5($2))")
        .bind(decoy).bind("other notes without the query term").execute(&pool).await.unwrap();
    let decoy_hash = format!("hy-{}", uuid::Uuid::new_v4());
    noted_db::chunks::upsert(&pool, &[(decoy_hash.clone(), "other notes without the query term".to_string(), 10)])
        .await.unwrap();
    noted_db::chunks::set_page_chunks(&pool, decoy, &[decoy_hash.clone()]).await.unwrap();
    noted_db::chunks::store_embedding(&pool, &decoy_hash, "other-model", &vec_at(5)).await.unwrap();

    let hits = search::hybrid(&pool, ws, "irrelevant-query-term", &vec_at(5), &model, 10).await.unwrap();
    assert!(
        hits.iter().any(|h| h.page_id == target),
        "the target embedding under the queried model must be found by the vector arm"
    );
    assert!(
        !hits.iter().any(|h| h.page_id == decoy),
        "an embedding under a DIFFERENT model must never be compared, even with an identical vector"
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
    let model = unique_model();
    let both = page_with(&pool, ws, "Both", "postgres postgres tuning advice", 5, &model).await;
    let lex_only = page_with(&pool, ws, "LexOnly", "postgres unrelated topic", 600, &model).await;

    let hits = search::hybrid(&pool, ws, "postgres", &vec_at(5), &model, 10).await.unwrap();
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
    let model = unique_model();
    let secret = page_with(&pool, ws_b, "Secret", "acquisition terms and price", 9, &model).await;

    let hits = search::hybrid(&pool, ws_a, "acquisition", &vec_at(9), &model, 10).await.unwrap();
    assert!(
        !hits.iter().any(|h| h.page_id == secret),
        "search must never cross a workspace boundary"
    );
}

#[tokio::test]
async fn an_empty_query_returns_nothing() {
    let (pool, ws) = setup().await;
    let model = unique_model();
    let _ = page_with(&pool, ws, "Thing", "some text here", 0, &model).await;
    let hits = search::hybrid(&pool, ws, "  ", &vec_at(0), &model, 10).await.unwrap();
    assert!(hits.is_empty(), "a blank query must not dump the workspace");
}

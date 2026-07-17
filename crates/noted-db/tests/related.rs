use noted_db::search;

const M: &str = "test-model";

async fn setup() -> (noted_db::PgPool, uuid::Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (name) VALUES ('rel-test') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    (pool, ws)
}

/// A one-hot vector. `axis` picks which dimension is hot, so "similarity" is
/// exactly controllable: same axis => cosine distance 0, different axis => 1.
fn axis_vec(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; 768];
    v[axis] = 1.0;
    v
}

/// Build a page whose chunks have the given embeddings under `model`, in the
/// given order — so `chunks[0]` is the page's FIRST chunk, the one
/// `related_pages` compares from.
async fn page_with_vecs_model(
    pool: &noted_db::PgPool, ws: uuid::Uuid, title: &str,
    chunks: &[(&str, Vec<f32>)], model: &str,
) -> uuid::Uuid {
    let page: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
        .bind(ws).bind(title).fetch_one(pool).await.unwrap();
    let mut hashes = Vec::new();
    for (text, v) in chunks {
        let hash = format!("rel-{}", uuid::Uuid::new_v4());
        noted_db::chunks::upsert(pool, &[(hash.clone(), text.to_string(), 10)]).await.unwrap();
        noted_db::chunks::store_embedding(pool, &hash, model, v).await.unwrap();
        hashes.push(hash);
    }
    noted_db::chunks::set_page_chunks(pool, page, &hashes).await.unwrap();
    page
}

/// Build a page whose single chunk has a known embedding. `axis` picks which
/// dimension is hot, so "similarity" is exactly controllable.
async fn page_with_vec(
    pool: &noted_db::PgPool, ws: uuid::Uuid, title: &str, text: &str, axis: usize,
) -> uuid::Uuid {
    page_with_vecs_model(pool, ws, title, &[(text, axis_vec(axis))], M).await
}

#[tokio::test]
async fn a_similar_page_ranks_above_a_dissimilar_one() {
    let (pool, ws) = setup().await;
    let source = page_with_vec(&pool, ws, "Source", "about postgres", 0).await;
    let near = page_with_vec(&pool, ws, "Near", "also about postgres", 0).await;
    let far = page_with_vec(&pool, ws, "Far", "about knitting", 500).await;

    let hits = search::related_pages(&pool, source, M, 10).await.unwrap();
    assert!(!hits.is_empty(), "related must return something");

    let near_pos = hits.iter().position(|h| h.page_id == near);
    let far_pos = hits.iter().position(|h| h.page_id == far);
    assert!(near_pos.is_some(), "the similar page must be returned");
    if let (Some(n), Some(f)) = (near_pos, far_pos) {
        assert!(n < f, "the similar page must rank above the dissimilar one");
    }
}

/// The panel is for pages you have NOT already linked — showing the page you
/// are reading is noise.
#[tokio::test]
async fn the_source_page_is_excluded_from_its_own_related_list() {
    let (pool, ws) = setup().await;
    let source = page_with_vec(&pool, ws, "Source", "about postgres", 0).await;
    let _other = page_with_vec(&pool, ws, "Other", "also postgres", 0).await;

    let hits = search::related_pages(&pool, source, M, 10).await.unwrap();
    assert!(
        !hits.iter().any(|h| h.page_id == source),
        "a page must never be related to itself"
    );
}

#[tokio::test]
async fn related_is_scoped_to_the_workspace() {
    let (pool, ws_a) = setup().await;
    let (_, ws_b) = setup().await;
    let source = page_with_vec(&pool, ws_a, "Source", "about postgres", 0).await;
    let foreign = page_with_vec(&pool, ws_b, "Foreign", "about postgres", 0).await;

    let hits = search::related_pages(&pool, source, M, 10).await.unwrap();
    assert!(
        !hits.iter().any(|h| h.page_id == foreign),
        "related must never cross a workspace boundary"
    );
}

/// Two models' vectors coexist in `embeddings` (model_id is in the PK, so a
/// live re-embed leaves both). Vectors from different models are not
/// comparable, so a page embedded under another model must never surface —
/// however close it looks numerically.
///
/// This test deliberately gives the SOURCE an embedding under the queried
/// model. An earlier version queried a model the source had no vectors for,
/// which made `EXISTS (SELECT 1 FROM src)` short-circuit the whole WHERE: it
/// passed vacuously, and would have passed identically with the `e.model_id`
/// filter deleted. Here `src` is non-empty, so the model filter is the ONLY
/// thing that can exclude the decoy.
#[tokio::test]
async fn related_only_compares_within_one_model() {
    let (pool, ws) = setup().await;
    let source = page_with_vec(&pool, ws, "Source", "about postgres", 0).await;
    // A genuine same-model neighbour. Without this the test could pass simply
    // because the query returns nothing at all.
    let same_model = page_with_vec(&pool, ws, "Same model", "also about postgres", 0).await;
    // Near-identical vector to the source's — but stored under a DIFFERENT model.
    let decoy = page_with_vecs_model(
        &pool, ws, "Decoy", &[("about postgres too", axis_vec(0))], "a-different-model",
    ).await;

    let hits = search::related_pages(&pool, source, M, 10).await.unwrap();
    assert!(
        hits.iter().any(|h| h.page_id == same_model),
        "the same-model neighbour must be returned, else this test proves nothing"
    );
    assert!(
        !hits.iter().any(|h| h.page_id == decoy),
        "a page embedded under a different model must never be returned"
    );
}

/// `RelatedHit` promises related PAGES, not related chunks. A page with 5
/// chunks must not become 5 rows that each compete for the LIMIT — that both
/// repeats the page in the panel and lets one chunky page crowd every other
/// page out of the results.
#[tokio::test]
async fn a_multi_chunk_page_appears_once() {
    let (pool, ws) = setup().await;
    let source = page_with_vec(&pool, ws, "Source", "about postgres", 0).await;

    // Three chunks, all NEAR the source (axis 0 dominant) but at increasing
    // distance, so there is a well-defined closest one.
    let near = |off: f32| {
        let mut v = axis_vec(0);
        v[1] = off;
        v
    };
    let multi = page_with_vecs_model(
        &pool, ws, "Multi",
        &[
            ("closest chunk", near(0.1)),
            ("middle chunk", near(0.3)),
            ("farthest chunk", near(0.5)),
        ],
        M,
    ).await;

    let hits = search::related_pages(&pool, source, M, 10).await.unwrap();
    let appearances: Vec<_> = hits.iter().filter(|h| h.page_id == multi).collect();
    assert_eq!(
        appearances.len(), 1,
        "a multi-chunk page must appear exactly once, got {} rows: {:?}",
        appearances.len(),
        hits.iter().map(|h| (&h.title, &h.snippet)).collect::<Vec<_>>()
    );
    assert_eq!(
        appearances[0].snippet, "closest chunk",
        "the surviving row must carry the page's CLOSEST chunk as its snippet"
    );
}

#[tokio::test]
async fn a_page_with_no_embeddings_returns_nothing() {
    let (pool, ws) = setup().await;
    let bare: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, title) VALUES ($1, 'Bare') RETURNING id")
        .bind(ws).fetch_one(&pool).await.unwrap();
    let hits = search::related_pages(&pool, bare, M, 10).await.unwrap();
    assert!(hits.is_empty(), "a page with no chunks must yield no related pages, not an error");
}

/// This is the whole point of the restructure: prove `embeddings_hnsw_idx` is
/// actually CHOSEN by the planner for the ANN shape `related_pages` runs. The
/// old query "looked" index-friendly too — it wasn't.
///
/// pgvector only pushes ANN ordering into the index for `ORDER BY <vec> <=>
/// <const> LIMIT n` taken DIRECTLY off the base-relation scan. The old shape
/// put a `GROUP BY` aggregate between the scan and the sort, so the planner had
/// to compute a distance for EVERY matching embedding before it could pick the
/// top N — a filtered full scan, which also made `hnsw.iterative_scan` inert
/// (no ANN ordering existed for it to rescue).
///
/// A tiny table is cheaper to seq-scan whatever the query shape, so an
/// assertion on a small fixture would prove nothing. 6,000 embeddings is enough
/// here for the planner to genuinely prefer the index (each row is 768 floats,
/// so the heap is already ~18MB — far heavier per row than Task 1's `pages`,
/// which needed 100k). `enable_seqscan` is NOT touched: this asserts the
/// planner's real preference, not mere index usability.
#[tokio::test]
async fn related_pages_uses_the_hnsw_index() {
    let (pool, ws) = setup().await;
    let source = page_with_vec(&pool, ws, "Source", "about postgres", 0).await;

    // `tag` doubles as each filler page's title AND its chunk's content_hash,
    // which is what lets page_chunks be built with a plain title join below.
    let tag = format!("hnsw-{}", uuid::Uuid::new_v4());

    sqlx::query(
        "INSERT INTO chunks (content_hash, text, token_estimate)
         SELECT $1 || '-' || g, 'filler chunk ' || g, 10 FROM generate_series(1, 6000) g",
    ).bind(&tag).execute(&pool).await.unwrap();

    sqlx::query(
        "INSERT INTO pages (workspace_id, title)
         SELECT $1, $2 || '-' || g FROM generate_series(1, 6000) g",
    ).bind(ws).bind(&tag).execute(&pool).await.unwrap();

    sqlx::query(
        "INSERT INTO page_chunks (page_id, chunk_index, content_hash)
         SELECT p.id, 0, p.title FROM pages p
         WHERE p.workspace_id = $1 AND p.title LIKE $2 || '-%'",
    ).bind(ws).bind(&tag).execute(&pool).await.unwrap();

    // The vector subquery is correlated with `g` (`WHERE d.i + g > 0`) ON
    // PURPOSE. Uncorrelated, Postgres hoists it to an InitPlan and evaluates it
    // ONCE — every row then gets the SAME vector, and an HNSW index over a
    // single distinct point is degenerate and proves nothing about a real ANN
    // scan. This was observed, not theorised.
    sqlx::query(
        "INSERT INTO embeddings (content_hash, model_id, embedding)
         SELECT $1 || '-' || g, $2,
                (SELECT array_agg(random()) FROM generate_series(1, 768) AS d(i)
                 WHERE d.i + g > 0)::vector
         FROM generate_series(1, 6000) g",
    ).bind(&tag).bind(M).execute(&pool).await.unwrap();

    // Fresh statistics, or the planner costs these tables as if still empty.
    sqlx::query("ANALYZE embeddings").execute(&pool).await.unwrap();
    sqlx::query("ANALYZE pages").execute(&pool).await.unwrap();
    sqlx::query("ANALYZE page_chunks").execute(&pool).await.unwrap();
    sqlx::query("ANALYZE chunks").execute(&pool).await.unwrap();

    let distinct: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT embedding::text) FROM embeddings WHERE content_hash LIKE $1 || '-%'",
    ).bind(&tag).fetch_one(&pool).await.unwrap();
    assert_eq!(distinct, 6000, "the filler vectors must be distinct, else the index test is degenerate");

    // Both plans are taken inside a transaction with the same `SET LOCAL` that
    // `related_pages` uses, so the planner sees exactly the runtime settings.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL hnsw.iterative_scan = relaxed_order").execute(&mut *tx).await.unwrap();

    // --- BEFORE: the old shape. GROUP BY between the scan and the sort. ---
    let before_rows: Vec<(String,)> = sqlx::query_as(
        "EXPLAIN (FORMAT TEXT)
         WITH src AS (
             SELECT e.embedding FROM page_chunks pc
             JOIN embeddings e ON e.content_hash = pc.content_hash AND e.model_id = $2
             WHERE pc.page_id = $1
         ),
         ws AS (SELECT workspace_id FROM pages WHERE id = $1)
         SELECT p.id AS page_id, p.title, c.text AS snippet,
                MIN(e.embedding <=> (SELECT embedding FROM src LIMIT 1))::real AS distance
         FROM embeddings e
         JOIN chunks c        ON c.content_hash = e.content_hash
         JOIN page_chunks pc  ON pc.content_hash = c.content_hash
         JOIN pages p         ON p.id = pc.page_id
         WHERE e.model_id = $2
           AND p.id <> $1
           AND p.archived_at IS NULL
           AND p.workspace_id = (SELECT workspace_id FROM ws)
           AND EXISTS (SELECT 1 FROM src)
         GROUP BY p.id, p.title, c.text
         ORDER BY distance
         LIMIT $3",
    ).bind(source).bind(M).bind(10i64).fetch_all(&mut *tx).await.unwrap();
    let before_plan = before_rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>().join("\n");

    // --- AFTER: the `near` CTE's shape, exactly as `related_pages` runs it. ---
    let after_rows: Vec<(String,)> = sqlx::query_as(
        "EXPLAIN (FORMAT TEXT)
         WITH src AS (
             SELECT e.embedding FROM page_chunks pc
             JOIN embeddings e ON e.content_hash = pc.content_hash AND e.model_id = $2
             WHERE pc.page_id = $1
             ORDER BY pc.chunk_index
             LIMIT 1
         ),
         ws AS (SELECT workspace_id FROM pages WHERE id = $1)
         SELECT e.content_hash,
                (e.embedding <=> (SELECT embedding FROM src)) AS distance
         FROM embeddings e
         WHERE e.model_id = $2
           AND EXISTS (SELECT 1 FROM src)
           AND EXISTS (
               SELECT 1
               FROM page_chunks pc
               JOIN pages p ON p.id = pc.page_id
               WHERE pc.content_hash = e.content_hash
                 AND p.workspace_id = (SELECT workspace_id FROM ws)
                 AND p.id <> $1
                 AND p.archived_at IS NULL
               OFFSET 0
           )
         ORDER BY e.embedding <=> (SELECT embedding FROM src)
         LIMIT $3::bigint * 5",
    ).bind(source).bind(M).bind(10i64).fetch_all(&mut *tx).await.unwrap();
    let after_plan = after_rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>().join("\n");

    tx.commit().await.unwrap();

    eprintln!("=== BEFORE (old GROUP BY shape) plan ===\n{before_plan}");
    eprintln!("=== AFTER (near CTE shape) plan ===\n{after_plan}");

    assert!(
        !before_plan.contains("embeddings_hnsw_idx"),
        "sanity check failed: the OLD shape unexpectedly used the HNSW index; plan:\n{before_plan}"
    );
    assert!(
        after_plan.contains("embeddings_hnsw_idx"),
        "the FIXED ANN shape must use embeddings_hnsw_idx; plan:\n{after_plan}"
    );
    assert!(
        after_plan.contains("Order By: (embedding <=>"),
        "the index must supply the ANN ORDERING (not merely be scanned); plan:\n{after_plan}"
    );
}


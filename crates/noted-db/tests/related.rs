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

/// Build a page whose single chunk has a known embedding. `axis` picks which
/// dimension is hot, so "similarity" is exactly controllable.
async fn page_with_vec(
    pool: &noted_db::PgPool, ws: uuid::Uuid, title: &str, text: &str, axis: usize,
) -> uuid::Uuid {
    let page: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
        .bind(ws).bind(title).fetch_one(pool).await.unwrap();
    let hash = format!("rel-{}", uuid::Uuid::new_v4());
    noted_db::chunks::upsert(pool, &[(hash.clone(), text.to_string(), 10)]).await.unwrap();
    noted_db::chunks::set_page_chunks(pool, page, &[hash.clone()]).await.unwrap();
    let mut v = vec![0.0f32; 768];
    v[axis] = 1.0;
    noted_db::chunks::store_embedding(pool, &hash, M, &v).await.unwrap();
    page
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

/// Two models' vectors coexist. Comparing across them is meaningless.
#[tokio::test]
async fn related_only_compares_within_one_model() {
    let (pool, ws) = setup().await;
    let source = page_with_vec(&pool, ws, "Source", "about postgres", 0).await;
    let hits = search::related_pages(&pool, source, "a-model-with-no-vectors", 10).await.unwrap();
    assert!(hits.is_empty(), "a model with no embeddings must return nothing, not cross-model noise");
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

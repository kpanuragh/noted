//! M6-3 — semantic community selection.
use noted_db::community;
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let p = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&p).await.unwrap();
    p
}
async fn ws(p: &noted_db::PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('sem') RETURNING id")
        .fetch_one(p).await.unwrap()
}
async fn entity(p: &noted_db::PgPool, w: Uuid, n: &str) -> Uuid {
    noted_db::graph::resolve_entity(p, w, &format!("{n}-{}", Uuid::new_v4()), Some("CONCEPT"), None)
        .await.unwrap()
}

/// A community of `n` members with a summary and (optionally) an embedding.
async fn community_with(
    p: &noted_db::PgPool, w: Uuid, label: &str, n: usize,
    model: &str, summary: &str, embedding: Option<&[f32]>,
) -> Uuid {
    let mut members = Vec::new();
    for i in 0..n {
        members.push(entity(p, w, &format!("{label}{i}")).await);
    }
    // Append to the existing partition rather than replacing it.
    let rows: Vec<(Uuid, i32, Uuid)> = sqlx::query_as(
        "SELECT c.id, c.level, cm.entity_id FROM communities c
         JOIN community_members cm ON cm.community_id = c.id
         WHERE c.workspace_id = $1 ORDER BY c.id")
        .bind(w).fetch_all(p).await.unwrap();
    let mut groups: Vec<(i32, Vec<Uuid>)> = Vec::new();
    let mut cur: Option<Uuid> = None;
    for (cid, level, ent) in rows {
        if cur != Some(cid) { groups.push((level, Vec::new())); cur = Some(cid); }
        groups.last_mut().unwrap().1.push(ent);
    }
    groups.push((0, members.clone()));
    community::swap_partition(p, w, &groups).await.unwrap();

    let id: Uuid = sqlx::query_scalar(
        "SELECT c.id FROM communities c JOIN community_members cm ON cm.community_id = c.id
         WHERE c.workspace_id = $1 AND cm.entity_id = $2")
        .bind(w).bind(members[0]).fetch_one(p).await.unwrap();
    let hash: String = sqlx::query_scalar("SELECT member_set_hash FROM communities WHERE id = $1")
        .bind(id).fetch_one(p).await.unwrap();
    sqlx::query(
        "INSERT INTO community_summaries (community_id, model_id, summary, state, member_set_hash)
         VALUES ($1,$2,$3,'valid',$4) ON CONFLICT (community_id) DO UPDATE SET summary = EXCLUDED.summary")
        .bind(id).bind(model).bind(summary).bind(hash).execute(p).await.unwrap();

    if let Some(v) = embedding {
        community::store_summary_embedding(p, id, "embed-test", &format!("{:x}", md5_of(summary)), v)
            .await.unwrap();
    }
    id
}

fn md5_of(s: &str) -> u128 {
    // Only needs to be stable within a test; the production hash is Postgres's md5().
    s.bytes().fold(0u128, |a, b| a.wrapping_mul(31).wrapping_add(b as u128))
}

fn vec_at(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; 768];
    v[axis] = 1.0;
    v
}

/// **THE headline property: a question matching a SMALL theme selects that
/// theme, over a much larger irrelevant one.**
///
/// This is the test the old size-ranked implementation could not pass. It is
/// also, deliberately, the case where the summary uses DIFFERENT WORDS from the
/// question — embeddings are the only mechanism here that can bridge that, and
/// the lexical relevance scoring in `global_search` scores it zero.
#[tokio::test]
async fn a_small_theme_that_matches_the_question_beats_a_large_one_that_does_not() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let model = format!("sem-{}", Uuid::new_v4());

    // The BIG theme is irrelevant; the SMALL one is what the question is about.
    let _big = community_with(&pool, w, "big", 12, &model,
        "sourdough starter, hydration ratios and weekend baking",
        Some(&vec_at(700))).await;
    let small = community_with(&pool, w, "small", 2, &model,
        "database migration incidents and index rebuild contention",
        Some(&vec_at(11))).await;

    // The question's embedding sits on the small theme's axis.
    let sel = community::summaries_by_similarity(&pool, w, &model, "embed-test", &vec_at(11), 5)
        .await.unwrap();

    assert_eq!(
        sel.candidates.first().map(|c| c.community_id),
        Some(small),
        "the relevant theme must rank first even though it is six times smaller"
    );
}

/// Size no longer decides. Same fixture, ranked by the OTHER axis, and the
/// answer flips — proving the ordering follows the question rather than the
/// data's shape.
#[tokio::test]
async fn the_ranking_follows_the_question_not_the_member_count() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let model = format!("sem-{}", Uuid::new_v4());

    let big = community_with(&pool, w, "big", 12, &model, "baking", Some(&vec_at(700))).await;
    let small = community_with(&pool, w, "small", 2, &model, "databases", Some(&vec_at(11))).await;

    let toward_big = community::summaries_by_similarity(&pool, w, &model, "embed-test", &vec_at(700), 5)
        .await.unwrap();
    assert_eq!(toward_big.candidates.first().map(|c| c.community_id), Some(big));

    let toward_small = community::summaries_by_similarity(&pool, w, &model, "embed-test", &vec_at(11), 5)
        .await.unwrap();
    assert_eq!(toward_small.candidates.first().map(|c| c.community_id), Some(small));
}

/// A community whose summary is not embedded yet is COUNTED as skipped, not
/// silently dropped — the same honesty rule the size-based selection followed.
#[tokio::test]
async fn an_unembedded_summary_is_reported_as_skipped() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let model = format!("sem-{}", Uuid::new_v4());

    community_with(&pool, w, "yes", 3, &model, "embedded theme", Some(&vec_at(5))).await;
    community_with(&pool, w, "no", 3, &model, "not embedded yet", None).await;

    let sel = community::summaries_by_similarity(&pool, w, &model, "embed-test", &vec_at(5), 10)
        .await.unwrap();
    assert_eq!(sel.candidates.len(), 1);
    assert_eq!(sel.skipped_unsummarised, 1, "an answer must say what it could not read");
}

/// The work queue is a set difference: it returns what needs embedding and
/// nothing else, and goes quiet once the work is done.
#[tokio::test]
async fn the_embedding_queue_drains_and_stays_drained() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let model = format!("sem-{}", Uuid::new_v4());
    let id = community_with(&pool, w, "a", 3, &model, "a theme about databases", None).await;

    let pending = community::summaries_needing_embedding(&pool, w, "embed-test", &model, 10)
        .await.unwrap();
    assert_eq!(pending.len(), 1, "an unembedded summary must be queued");
    assert_eq!(pending[0].0, id);

    community::store_summary_embedding(&pool, id, "embed-test", "hash-1", &vec_at(3))
        .await.unwrap();

    // Stored with a hash matching the CURRENT summary text.
    sqlx::query("UPDATE community_summary_embeddings SET summary_hash = md5($2)
                 WHERE community_id = $1")
        .bind(id).bind("a theme about databases").execute(&pool).await.unwrap();

    let after = community::summaries_needing_embedding(&pool, w, "embed-test", &model, 10)
        .await.unwrap();
    assert!(after.is_empty(), "an up-to-date summary must not be re-queued: {after:?}");
}

/// **A REGENERATED summary is re-embedded.**
///
/// MECHANISM PROTECTED: the `summary_hash IS DISTINCT FROM md5(s.summary)`
/// clause. Without it a summary rewritten by a new model keeps a vector
/// describing prose that no longer exists — and semantic selection silently
/// ranks by the old meaning forever.
#[tokio::test]
async fn a_rewritten_summary_is_re_embedded() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let model = format!("sem-{}", Uuid::new_v4());
    let id = community_with(&pool, w, "a", 3, &model, "the original summary", None).await;

    community::store_summary_embedding(&pool, id, "embed-test", "stale", &vec_at(3)).await.unwrap();
    sqlx::query("UPDATE community_summary_embeddings SET summary_hash = md5($2)
                 WHERE community_id = $1")
        .bind(id).bind("the original summary").execute(&pool).await.unwrap();
    assert!(community::summaries_needing_embedding(&pool, w, "embed-test", &model, 10)
        .await.unwrap().is_empty(), "premise: up to date");

    // The summariser rewrites it.
    sqlx::query("UPDATE community_summaries SET summary = $2 WHERE community_id = $1")
        .bind(id).bind("a completely different summary").execute(&pool).await.unwrap();

    let pending = community::summaries_needing_embedding(&pool, w, "embed-test", &model, 10)
        .await.unwrap();
    assert_eq!(pending.len(), 1, "a rewritten summary must be re-embedded");
}

/// Another workspace's themes are never selected.
#[tokio::test]
async fn selection_never_crosses_a_workspace() {
    let pool = pool().await;
    let mine = ws(&pool).await;
    let theirs = ws(&pool).await;
    let model = format!("sem-{}", Uuid::new_v4());

    community_with(&pool, theirs, "t", 5, &model, "their theme", Some(&vec_at(9))).await;

    let sel = community::summaries_by_similarity(&pool, mine, &model, "embed-test", &vec_at(9), 10)
        .await.unwrap();
    assert!(sel.candidates.is_empty(), "tenancy leak");
    assert_eq!(sel.skipped_unsummarised, 0, "and not even counted");
}

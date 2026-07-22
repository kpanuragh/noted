//! M2c Task 3 — global (theme-anchored) search.
//!
//! Every fixture gets its OWN workspace and a `model_id` unique to its test.
//! Tests share a dev database, so nothing here may assert anything
//! instance-wide — `materialize.rs` carries the scar from the last time
//! something did.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use noted_index::answer::{AnswerError, AnswerProvider, AnswerRequest, StubAnswerer};
use noted_index::global_search::{GlobalSearchError, global_search};
use noted_index::summary::{CommunityFacts, StubSummariser, SummaryError, SummaryProvider};
use uuid::Uuid;

// ---------------------------------------------------------------- fixtures --

async fn connect() -> noted_db::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

async fn workspace(pool: &noted_db::PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('global-search-test') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn entity(pool: &noted_db::PgPool, ws: Uuid, label: &str) -> Uuid {
    noted_db::graph::resolve_entity(
        pool,
        ws,
        &format!("{label}-{}", Uuid::new_v4()),
        Some("CONCEPT"),
        None,
    )
    .await
    .unwrap()
}

/// Append ONE community to the workspace's partition, keeping the ones already
/// there.
///
/// `swap_partition` REPLACES a workspace's whole partition, so building
/// communities with repeated calls would delete each previous one — and cascade
/// its summary away with it. Appending re-submits the existing member sets
/// alongside the new one, which keeps every `(level, member_set_hash)` stable
/// and therefore every community id and summary intact. That is exactly the
/// identity preservation M2b's swap was built for, so these fixtures lean on it
/// rather than working around it.
async fn add_community(pool: &noted_db::PgPool, ws: Uuid, members: Vec<Uuid>) -> Uuid {
    let rows: Vec<(Uuid, i32, Uuid)> = sqlx::query_as(
        "SELECT c.id, c.level, cm.entity_id
         FROM communities c
         JOIN community_members cm ON cm.community_id = c.id
         WHERE c.workspace_id = $1
         ORDER BY c.id",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap();

    let mut groups: Vec<(i32, Vec<Uuid>)> = Vec::new();
    let mut current: Option<Uuid> = None;
    for (cid, level, ent) in rows {
        if current != Some(cid) {
            groups.push((level, Vec::new()));
            current = Some(cid);
        }
        groups.last_mut().unwrap().1.push(ent);
    }
    groups.push((0, members.clone()));

    noted_db::community::swap_partition(pool, ws, &groups)
        .await
        .unwrap();

    sqlx::query_scalar(
        "SELECT c.id FROM communities c
         JOIN community_members cm ON cm.community_id = c.id
         WHERE c.workspace_id = $1 AND cm.entity_id = $2",
    )
    .bind(ws)
    .bind(members[0])
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A community with `n` members and a summary, through the production writers.
async fn community_with_summary(
    pool: &noted_db::PgPool,
    ws: Uuid,
    label: &str,
    n: usize,
    model: &str,
    summary: &str,
    state: &str,
) -> Uuid {
    let mut members = Vec::with_capacity(n);
    for i in 0..n {
        members.push(entity(pool, ws, &format!("{label}{i}")).await);
    }
    let id = add_community(pool, ws, members).await;

    let current: String =
        sqlx::query_scalar("SELECT member_set_hash FROM communities WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();

    // A stale summary's stored hash must DISAGREE with its community's, because
    // that disagreement — not the `state` column — is what
    // `pending_summaries` actually tests. Writing `state = 'stale_usable'` while
    // leaving the hashes equal builds a row production can never produce: the
    // hot path updates the community's hash in place and leaves the summary's
    // behind, which is precisely what "stale" means here. A fixture that sets
    // only the flag looks stale to a reader and current to the code.
    let hash = if state == "stale_usable" {
        format!("stale-{}", Uuid::new_v4())
    } else {
        current
    };

    sqlx::query(
        "INSERT INTO community_summaries (community_id, model_id, summary, state, member_set_hash)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (community_id) DO UPDATE
           SET summary = EXCLUDED.summary, state = EXCLUDED.state, model_id = EXCLUDED.model_id",
    )
    .bind(id)
    .bind(model)
    .bind(summary)
    .bind(state)
    .bind(hash)
    .execute(pool)
    .await
    .unwrap();

    id
}

// ---------------------------------------------------------------- providers --

struct Counting {
    calls: AtomicUsize,
    inner: StubAnswerer,
}

impl Counting {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            inner: StubAnswerer::new(),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl AnswerProvider for Counting {
    fn model_id(&self) -> &str {
        "counting-answerer"
    }
    async fn synthesise(&self, req: &AnswerRequest) -> Result<String, AnswerError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.synthesise(req).await
    }
}

/// Counts how many times a summary was actually regenerated, so the lazy
/// refresh trigger can be asserted rather than assumed.
struct CountingSummariser {
    calls: AtomicUsize,
    inner: StubSummariser,
    model: String,
}

impl CountingSummariser {
    fn new(model: &str) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            inner: StubSummariser::new(),
            model: model.to_string(),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl SummaryProvider for CountingSummariser {
    fn model_id(&self) -> &str {
        &self.model
    }
    async fn summarise(&self, facts: &CommunityFacts) -> Result<String, SummaryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.summarise(facts).await
    }
}

// ------------------------------------------------------------------- tests --

/// The shape of the surface: map over each summarised theme, reduce to one
/// answer, and report every theme that contributed.
///
/// MECHANISM PROTECTED: the map loop and the reduce call in `global_search`.
/// Pinned to a single call, or to skipping the reduce, this fails on the call
/// count.
#[tokio::test]
async fn every_summarised_theme_contributes_a_partial_and_one_reduce_runs() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let model = format!("gs-{}", Uuid::new_v4());

    community_with_summary(
        &pool,
        ws,
        "pg",
        4,
        &model,
        "postgres tuning and checkpoint storms",
        "valid",
    )
    .await;
    community_with_summary(
        &pool,
        ws,
        "bread",
        3,
        &model,
        "sourdough starter and weekend baking",
        "valid",
    )
    .await;

    let answerer = Counting::new();
    let summariser = Arc::new(CountingSummariser::new(&model));

    let ans = global_search(
        &pool,
        ws,
        "postgres checkpoint tuning",
        &answerer,
        summariser.clone(),
    )
    .await
    .unwrap();

    assert_eq!(ans.partials.len(), 2, "both themes must contribute");
    // Sanity before the `==` count assertion, so it cannot pass because nothing ran.
    assert!(!ans.answer.is_empty(), "expected a reduced answer");
    assert_eq!(
        answerer.calls(),
        3,
        "one map call per community plus exactly one reduce"
    );
    assert_eq!(
        ans.skipped_unsummarised, 0,
        "every community here has a summary"
    );
}

/// Relevance ORDERS the partials, and the reduce step sees that order.
///
/// MECHANISM PROTECTED: the `partials.sort_by` in `global_search`. Removed, the
/// insertion order (size-ranked, so the larger irrelevant theme comes first)
/// survives and this fails.
#[tokio::test]
async fn partials_reach_the_reducer_in_relevance_order_not_size_order() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let model = format!("gs-{}", Uuid::new_v4());

    // The BIGGER community is the IRRELEVANT one, so size order and relevance
    // order genuinely disagree. Without that, this test would pass under either.
    community_with_summary(
        &pool,
        ws,
        "big",
        9,
        &model,
        "sourdough starter and weekend baking",
        "valid",
    )
    .await;
    let small = community_with_summary(
        &pool,
        ws,
        "small",
        2,
        &model,
        "postgres checkpoint tuning notes",
        "valid",
    )
    .await;

    let answerer = Counting::new();
    let summariser = Arc::new(CountingSummariser::new(&model));

    let ans = global_search(
        &pool,
        ws,
        "postgres checkpoint tuning",
        &answerer,
        summariser,
    )
    .await
    .unwrap();

    assert_eq!(ans.partials.len(), 2);
    assert_eq!(
        ans.partials[0].community_id, small,
        "the theme that bears on the question must rank first even though it is smaller"
    );
    assert!(
        ans.partials[0].relevance > ans.partials[1].relevance,
        "relevance must be strictly ordered, got {:?}",
        ans.partials.iter().map(|p| p.relevance).collect::<Vec<_>>()
    );
}

/// A `stale_usable` summary is USED, and its regeneration is REQUESTED.
///
/// This is the only test in the product that exercises `SummaryWorker::refresh`
/// at all — M2b built the lazy path and nothing called it until global search
/// existed.
///
/// MECHANISM PROTECTED: the stale-collection branch and the refresh loop.
/// Deleting either leaves the summariser call count at 0 and this fails.
#[tokio::test]
async fn a_stale_summary_is_used_and_its_refresh_is_triggered() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let model = format!("gs-{}", Uuid::new_v4());

    let drifted = community_with_summary(
        &pool,
        ws,
        "drift",
        4,
        &model,
        "postgres checkpoint tuning notes",
        "stale_usable",
    )
    .await;

    let answerer = Counting::new();
    let summariser = Arc::new(CountingSummariser::new(&model));

    let ans = global_search(
        &pool,
        ws,
        "postgres checkpoint tuning",
        &answerer,
        summariser.clone(),
    )
    .await
    .unwrap();

    assert_eq!(
        ans.partials.len(),
        1,
        "USED: a stale summary beats a missing one (M2b 2.2)"
    );
    assert_eq!(ans.partials[0].community_id, drifted);
    assert!(
        ans.partials[0].was_stale,
        "the caveat must reach the caller, not be swallowed"
    );
    assert_eq!(
        summariser.calls(),
        1,
        "REQUESTED: the stale summary must have been regenerated exactly once"
    );

    let state: String =
        sqlx::query_scalar("SELECT state FROM community_summaries WHERE community_id = $1")
            .bind(drifted)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "valid", "the refresh must have cleared the stale mark");
}

/// A current summary costs ZERO summariser calls.
///
/// The counterpart to the test above: without this, "refresh everything every
/// search" would pass that one and quietly make every global search pay for a
/// full regeneration.
#[tokio::test]
async fn a_current_summary_costs_exactly_zero_regenerations() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let model = format!("gs-{}", Uuid::new_v4());

    community_with_summary(
        &pool,
        ws,
        "fresh",
        4,
        &model,
        "postgres checkpoint tuning notes",
        "valid",
    )
    .await;

    let answerer = Counting::new();
    let summariser = Arc::new(CountingSummariser::new(&model));

    let ans = global_search(
        &pool,
        ws,
        "postgres checkpoint tuning",
        &answerer,
        summariser.clone(),
    )
    .await
    .unwrap();

    // Sanity first: the search really did run and really did read this summary.
    assert_eq!(ans.partials.len(), 1, "the current summary must be used");
    assert_eq!(
        summariser.calls(),
        0,
        "a summary that is still valid must NOT be regenerated"
    );
}

/// Communities without a usable summary are counted, not hidden.
///
/// MECHANISM PROTECTED: `skipped_unsummarised`. Hardcoded to 0, this fails.
#[tokio::test]
async fn communities_with_no_summary_are_reported_as_skipped() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let model = format!("gs-{}", Uuid::new_v4());

    community_with_summary(&pool, ws, "has", 3, &model, "postgres tuning", "valid").await;
    // Two communities with members but no summary row at all.
    for label in ["bare1", "bare2"] {
        let members = vec![
            entity(&pool, ws, &format!("{label}a")).await,
            entity(&pool, ws, &format!("{label}b")).await,
        ];
        add_community(&pool, ws, members).await;
    }

    let answerer = Counting::new();
    let summariser = Arc::new(CountingSummariser::new(&model));

    let ans = global_search(&pool, ws, "postgres tuning", &answerer, summariser)
        .await
        .unwrap();

    assert_eq!(ans.partials.len(), 1);
    assert_eq!(
        ans.skipped_unsummarised, 2,
        "an answer over 1 of 3 themes must SAY it covered 1 of 3"
    );
}

/// A summary written by a DIFFERENT summariser is not consulted, and counts as
/// skipped — M2b already treats a model change as a full regeneration.
#[tokio::test]
async fn another_models_summary_is_not_consulted() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let mine = format!("gs-mine-{}", Uuid::new_v4());
    let theirs = format!("gs-theirs-{}", Uuid::new_v4());

    community_with_summary(
        &pool,
        ws,
        "other",
        4,
        &theirs,
        "postgres checkpoint tuning notes",
        "valid",
    )
    .await;

    let answerer = Counting::new();
    let summariser = Arc::new(CountingSummariser::new(&mine));

    let ans = global_search(
        &pool,
        ws,
        "postgres checkpoint tuning",
        &answerer,
        summariser,
    )
    .await
    .unwrap();

    assert!(
        ans.partials.is_empty(),
        "a summary from another model must not be presented as this model's"
    );
    assert_eq!(
        ans.skipped_unsummarised, 1,
        "and it must be counted as unconsulted rather than vanishing"
    );
    assert_eq!(
        answerer.calls(),
        0,
        "no material means the answerer is never invoked"
    );
}

/// Another workspace's summaries are never consulted.
#[tokio::test]
async fn global_search_never_reads_another_workspaces_themes() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let foreign = workspace(&pool).await;
    let model = format!("gs-{}", Uuid::new_v4());

    community_with_summary(
        &pool,
        foreign,
        "secret",
        6,
        &model,
        "postgres checkpoint tuning notes",
        "valid",
    )
    .await;

    let answerer = Counting::new();
    let summariser = Arc::new(CountingSummariser::new(&model));

    let ans = global_search(
        &pool,
        ws,
        "postgres checkpoint tuning",
        &answerer,
        summariser,
    )
    .await
    .unwrap();

    assert!(ans.partials.is_empty(), "tenancy leak");
    assert_eq!(ans.skipped_unsummarised, 0, "and not even counted");
    assert_eq!(answerer.calls(), 0);
}

/// An empty workspace gets a statement, not a hallucination — and the answerer
/// is never called.
#[tokio::test]
async fn no_summaries_means_no_provider_call() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let model = format!("gs-{}", Uuid::new_v4());

    let answerer = Counting::new();
    let summariser = Arc::new(CountingSummariser::new(&model));

    let ans = global_search(&pool, ws, "anything at all", &answerer, summariser)
        .await
        .unwrap();

    assert!(ans.partials.is_empty());
    assert_eq!(
        answerer.calls(),
        0,
        "a model handed a question and no material answers it from its weights"
    );
    assert!(
        ans.answer.contains("no summarised themes"),
        "the emptiness must be stated plainly, got: {}",
        ans.answer
    );
}

/// An empty answer from the reducer is refused rather than presented.
#[tokio::test]
async fn an_empty_reduced_answer_is_refused() {
    struct EmptyOutput;
    #[async_trait::async_trait]
    impl AnswerProvider for EmptyOutput {
        fn model_id(&self) -> &str {
            "empty-answerer"
        }
        async fn synthesise(&self, _req: &AnswerRequest) -> Result<String, AnswerError> {
            Ok("   \n ".into())
        }
    }

    let pool = connect().await;
    let ws = workspace(&pool).await;
    let model = format!("gs-{}", Uuid::new_v4());
    community_with_summary(&pool, ws, "t", 3, &model, "postgres tuning", "valid").await;

    let summariser = Arc::new(CountingSummariser::new(&model));
    let err = global_search(&pool, ws, "postgres tuning", &EmptyOutput, summariser)
        .await
        .unwrap_err();

    assert!(
        matches!(err, GlobalSearchError::Answer(_)),
        "an empty answer must surface as an error, not as an empty answer, got {err:?}"
    );
}

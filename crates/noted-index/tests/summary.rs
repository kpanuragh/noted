//! M2b Task 4 — `SummaryProvider`, the set-difference summary queue, and lazy
//! invalidation by `member_set_hash`.
//!
//! Every database test scopes its fixtures to its OWN freshly-created
//! workspace, and every chunk hash carries a per-test `run` marker. That is not
//! tidiness: this crate's tests run against a shared dev database that once
//! accumulated 754,906 junk rows, summarising is a WHOLE-WORKSPACE operation,
//! and `chunks` is content-addressed and therefore genuinely global (M1b) — an
//! unscoped fixture would summarise every entity any earlier test ever created.
use noted_index::community_worker::CommunityWorker;
use noted_index::summary::{
    CommunityFacts, CommunityMember, StubSummariser, SummaryError, SummaryProvider, verify_summary,
};
use noted_index::summary_worker::{
    STALE_USABLE_MIN_MEMBERS, STATE_STALE_USABLE, STATE_VALID, SummaryWorker, Urgency, classify,
    pending_summaries,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('summary-worker-test') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn page(pool: &noted_db::PgPool, ws: Uuid) -> Uuid {
    sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, 'p') RETURNING id")
        .bind(ws)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Write a graph through the repository primitives, with one live chunk on
/// `pg` supplying the provenance every clusterable edge needs. Mirrors
/// `tests/communities.rs::seed_graph`; `creation_order` exists so a test can
/// build the SAME graph twice under two different `gen_random_uuid()` orders.
async fn seed_graph(
    pool: &noted_db::PgPool,
    ws: Uuid,
    pg: Uuid,
    run: &str,
    tag: &str,
    creation_order: &[&str],
    edges: &[(&str, &str, f32)],
) -> HashMap<String, Uuid> {
    let hash = format!("sw-{run}-{tag}");
    noted_db::chunks::upsert(pool, &[(hash.clone(), format!("seed {run} {tag}"), 10)])
        .await
        .unwrap();

    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT content_hash FROM page_chunks WHERE page_id = $1 ORDER BY chunk_index",
    )
    .bind(pg)
    .fetch_all(pool)
    .await
    .unwrap();
    let mut all = existing;
    if !all.contains(&hash) {
        all.push(hash.clone());
    }
    noted_db::chunks::set_page_chunks(pool, pg, &all)
        .await
        .unwrap();

    let mut ids: HashMap<String, Uuid> = HashMap::new();
    for name in creation_order
        .iter()
        .copied()
        .chain(edges.iter().flat_map(|(a, b, _)| [*a, *b]))
    {
        if ids.contains_key(name) {
            continue;
        }
        let id = noted_db::graph::resolve_entity(pool, ws, name, Some("CONCEPT"), None)
            .await
            .unwrap();
        ids.insert(name.to_string(), id);
    }

    let tuples: Vec<(Uuid, Uuid, String, f32)> = edges
        .iter()
        .map(|(a, b, w)| (ids[*a], ids[*b], "mentions_with".to_string(), *w))
        .collect();
    noted_db::graph::replace_chunk_edges(pool, ws, &hash, "seeded-model", &tuples)
        .await
        .unwrap();

    ids
}

/// Two 4-cliques joined by one bridge edge: exactly two communities, so "which
/// summary landed where" is a question with an unambiguous answer.
const TWO_CLIQUES: &[(&str, &str, f32)] = &[
    ("aa", "ab", 1.0),
    ("aa", "ac", 1.0),
    ("aa", "ad", 1.0),
    ("ab", "ac", 1.0),
    ("ab", "ad", 1.0),
    ("ac", "ad", 1.0),
    ("xa", "xb", 1.0),
    ("xa", "xc", 1.0),
    ("xa", "xd", 1.0),
    ("xb", "xc", 1.0),
    ("xb", "xd", 1.0),
    ("xc", "xd", 1.0),
    ("ad", "xa", 1.0),
];
const TWO_CLIQUE_NODES: &[&str] = &["aa", "ab", "ac", "ad", "xa", "xb", "xc", "xd"];

/// A complete graph on `names` — the one shape whose optimal partition is a
/// SINGLE community, which is what a test about a LARGE community needs.
fn clique(names: &[String]) -> Vec<(&str, &str, f32)> {
    let mut out = Vec::new();
    for i in 0..names.len() {
        for j in (i + 1)..names.len() {
            out.push((names[i].as_str(), names[j].as_str(), 1.0f32));
        }
    }
    out
}

/// `n` names sorted-stable and lowercase — lowercase because the rest of this
/// suite's fixtures are, and `NN` keeps `name` ordering equal to index ordering.
fn names(prefix: &str, n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{prefix}{i:02}")).collect()
}

/// The stub, wrapped in a call counter and a per-test `model_id`.
///
/// The counter is the whole point of several tests below and the assertions
/// against it are `==`, never `<=`. M1b shipped a counter assertion written
/// `<=`, which passes vacuously for every implementation including one that
/// makes no calls at all and one that makes the wrong ones.
struct Counting {
    model: String,
    calls: AtomicUsize,
    inner: StubSummariser,
}

impl Counting {
    fn new(model: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            model: model.into(),
            calls: AtomicUsize::new(0),
            inner: StubSummariser::new(),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl SummaryProvider for Counting {
    fn model_id(&self) -> &str {
        &self.model
    }
    async fn summarise(&self, facts: &CommunityFacts) -> Result<String, SummaryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.summarise(facts).await
    }
}

/// Fails for any community containing `poison`, succeeds otherwise. The stub
/// deliberately never fails (a stub that failed on a magic name would be its
/// own trap), so the failure-isolation path gets a purpose-built provider —
/// M2a's lesson about unreachable production paths, applied up front rather
/// than retroactively.
struct FailsOnPoison(AtomicUsize);

#[async_trait::async_trait]
impl SummaryProvider for FailsOnPoison {
    fn model_id(&self) -> &str {
        "fails-on-poison"
    }
    async fn summarise(&self, facts: &CommunityFacts) -> Result<String, SummaryError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        if facts.members.iter().any(|m| m.name.starts_with("poison")) {
            return Err(SummaryError::Model("the model fell over".into()));
        }
        StubSummariser::new().summarise(facts).await
    }
}

/// "Succeeds" with nothing in it — a real model that ran out of context,
/// refused, or emitted only a fence that got stripped.
struct EmptyOutput;

#[async_trait::async_trait]
impl SummaryProvider for EmptyOutput {
    fn model_id(&self) -> &str {
        "empty-output"
    }
    async fn summarise(&self, _facts: &CommunityFacts) -> Result<String, SummaryError> {
        Ok("   \n\t ".into())
    }
}

/// Stored summaries for a workspace, keyed by the community's sorted member
/// names — never by community id, which is a random uuid that differs between
/// two workspaces holding the same graph.
async fn stored(
    pool: &noted_db::PgPool,
    ws: Uuid,
) -> HashMap<Vec<String>, (String, String, String)> {
    let rows: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT c.id, s.summary, s.state, s.model_id
         FROM communities c
         JOIN community_summaries s ON s.community_id = c.id
         WHERE c.workspace_id = $1",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap();

    let mut out = HashMap::new();
    for (cid, summary, state, model_id) in rows {
        let mut members: Vec<String> = sqlx::query_scalar(
            "SELECT e.name FROM community_members cm JOIN entities e ON e.id = cm.entity_id
             WHERE cm.community_id = $1",
        )
        .bind(cid)
        .fetch_all(pool)
        .await
        .unwrap();
        members.sort();
        out.insert(members, (summary, state, model_id));
    }
    out
}

/// The id of the community currently holding `name`.
async fn community_holding(pool: &noted_db::PgPool, ws: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar(
        "SELECT cm.community_id
         FROM entities e
         JOIN community_members cm ON cm.entity_id = e.id
         JOIN communities c        ON c.id = cm.community_id AND c.workspace_id = $1
         WHERE e.workspace_id = $1 AND e.name = $2",
    )
    .bind(ws)
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ------------------------------------------------- the provider and helpers --

/// The stub must VARY. M2a's `StubExtractor` was so uniform that four
/// production paths were unreachable by any test using it and had to be covered
/// retroactively with hand-built fixtures; the lesson recorded in
/// `.superpowers/sdd/progress.md` is that a constant stub hides whole classes
/// of wiring bug — most obviously "summary *i* stored against community *j*",
/// which no assertion can catch when every summary is the same string.
///
/// Three axes are pinned here because three separate behaviours depend on them:
/// membership (so summaries are distinguishable), ORDER (so a facts query that
/// stopped sorting by name is visible as changed prose), and SHAPE by size (so
/// a size-dependent path has two structurally different outputs to tell apart).
#[tokio::test]
async fn the_stub_summariser_varies_with_membership_order_and_size() {
    let stub = StubSummariser::new();
    let member = |n: &str, d: Option<&str>| CommunityMember {
        id: Uuid::new_v4(),
        name: n.into(),
        entity_type: "CONCEPT".into(),
        description: d.map(str::to_string),
    };
    let facts = |members: Vec<CommunityMember>| CommunityFacts {
        community_id: Uuid::new_v4(),
        level: 0,
        members,
    };

    let empty = stub.summarise(&facts(vec![])).await.unwrap();
    let single = stub
        .summarise(&facts(vec![member("alpha", None)]))
        .await
        .unwrap();
    let pair = stub
        .summarise(&facts(vec![member("alpha", None), member("beta", None)]))
        .await
        .unwrap();
    let reversed = stub
        .summarise(&facts(vec![member("beta", None), member("alpha", None)]))
        .await
        .unwrap();
    let other = stub
        .summarise(&facts(vec![member("gamma", None), member("delta", None)]))
        .await
        .unwrap();

    assert_ne!(empty, single, "size must change the output");
    assert_ne!(single, pair, "size must change the output");
    assert_ne!(
        pair, other,
        "two different member sets must summarise differently, or a summary stored against the \
         wrong community is undetectable"
    );
    assert_ne!(
        pair, reversed,
        "member ORDER must change the output, so a facts query that stopped ordering by name is \
         visible rather than silent"
    );
    assert!(
        single.contains("stands alone") && pair.contains("2 members"),
        "a one-member community and a larger one must differ in SHAPE, not merely in length: \
         {single:?} / {pair:?}"
    );

    let typed = stub
        .summarise(&facts(vec![member("alpha", Some("a thing"))]))
        .await
        .unwrap();
    assert!(
        typed.contains("CONCEPT") && typed.contains("a thing"),
        "entity type and description must reach the provider, or they are silently dropped \
         between the database and the model: {typed:?}"
    );
}

/// `community_summaries.summary` is `text NOT NULL` with no CHECK, so nothing
/// downstream stops an empty string being stored as though it were a summary —
/// after which the set-difference queue sees a matching hash and the community
/// is permanently "summarised" with nothing in it.
#[test]
fn an_empty_or_whitespace_summary_is_rejected() {
    assert!(verify_summary("a real summary", "m").is_ok());
    assert!(matches!(
        verify_summary("", "m"),
        Err(SummaryError::Invalid(_))
    ));
    assert!(matches!(
        verify_summary(" \n\t ", "m"),
        Err(SummaryError::Invalid(_))
    ));
    let msg = verify_summary("", "my-model").unwrap_err().to_string();
    assert!(
        msg.contains("my-model"),
        "the error must name the model: {msg}"
    );
}

/// The classification table, without a database. Both sides of the boundary and
/// both overrides.
#[test]
fn the_classification_rules_are_exhaustive_at_the_boundary() {
    let big = STALE_USABLE_MIN_MEMBERS;
    assert_eq!(classify(true, false, big), Urgency::Lazy);
    assert_eq!(
        classify(true, false, big - 1),
        Urgency::Eager,
        "one member below the boundary is the small side"
    );
    assert_eq!(
        classify(false, false, big),
        Urgency::Eager,
        "a community with no summary has nothing to serve, whatever its size"
    );
    assert_eq!(
        classify(true, true, big),
        Urgency::Eager,
        "a model change is a full regeneration (design §3), not a membership drift"
    );
}

// ----------------------------------------------------------- the zero point --

/// THE ACCEPTANCE CRITERION. Unchanged `member_set_hash` ⇒ the summary is still
/// valid ⇒ **exactly zero** summariser calls, however many cold runs sweep over
/// the workspace.
///
/// The mechanism this protects is the staleness filter in
/// `pending_summaries`'s WHERE clause: delete it and every community is pending
/// on every pass, which is a model call per community per cold run — the
/// regeneration storm design §2.2 exists to prevent.
///
/// It also depends on `swap_partition` PRESERVING a community's row when
/// `(level, member_set_hash)` is unchanged. If the swap deleted and re-created
/// the rows, `community_summaries` would cascade away and every community would
/// come back summary-less; the delta below would be 2, not 0. So this is
/// simultaneously the Task 1 identity-preservation guarantee's only end-to-end
/// consumer.
///
/// The assertion is `==`, never `<=`. M1b shipped a `<=` counter assertion that
/// passed for every possible implementation.
#[tokio::test]
async fn an_unchanged_member_set_costs_exactly_zero_summariser_calls() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();
    seed_graph(&pool, ws, pg, &run, "g", TWO_CLIQUE_NODES, TWO_CLIQUES).await;

    let provider = Counting::new(format!("sum-zero-{run}"));
    let worker = SummaryWorker::new(pool.clone(), provider.clone(), ws);
    let communities = CommunityWorker::new(pool.clone(), ws);

    communities.cold_run().await.unwrap();
    let first = worker.run_once().await.unwrap();
    assert_eq!(
        (first.regenerated, first.marked_stale, first.failed),
        (2, 0, 0),
        "sanity: both communities start with no summary and must be generated eagerly"
    );
    assert_eq!(
        provider.calls(),
        2,
        "sanity: exactly one model call per community; without this the zero below could be zero \
         because nothing ever ran"
    );

    // Two more cold runs over an UNCHANGED graph. Each re-derives the same
    // partition, so every `(level, member_set_hash)` already exists and every
    // community row — and therefore every summary — survives.
    communities.cold_run().await.unwrap();
    communities.cold_run().await.unwrap();

    let second = worker.run_once().await.unwrap();
    assert_eq!(
        (second.regenerated, second.marked_stale, second.failed),
        (0, 0, 0),
        "nothing changed, so the queue must be empty — not merely cheap"
    );
    assert_eq!(
        provider.calls(),
        2,
        "EXACTLY zero further summariser calls after two cold runs over an unchanged graph"
    );

    assert!(
        pending_summaries(&pool, ws, provider.model_id())
            .await
            .unwrap()
            .is_empty(),
        "and the set-difference queue itself must be empty, so this is a property of the query \
         rather than of run_once happening to skip things"
    );
}

/// `refresh` is the lazy path's trigger and a global search would call it on
/// every community it touches. Calling it on a CURRENT summary must therefore
/// cost nothing — it consults the same set-difference queue rather than
/// regenerating on sight.
#[tokio::test]
async fn refreshing_a_current_summary_costs_exactly_zero_calls() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();
    seed_graph(&pool, ws, pg, &run, "g", TWO_CLIQUE_NODES, TWO_CLIQUES).await;

    let provider = Counting::new(format!("sum-refresh-{run}"));
    let worker = SummaryWorker::new(pool.clone(), provider.clone(), ws);
    CommunityWorker::new(pool.clone(), ws)
        .cold_run()
        .await
        .unwrap();
    worker.run_once().await.unwrap();
    let before = provider.calls();
    assert_eq!(before, 2, "sanity: both communities were summarised");

    let cid = community_holding(&pool, ws, "aa").await;
    assert!(
        !worker.refresh(cid).await.unwrap(),
        "a community whose summary is already current for this model must report no work done"
    );
    assert_eq!(
        provider.calls(),
        before,
        "and must have made EXACTLY zero model calls"
    );
}

// ------------------------------------------------- eager / lazy classification --

/// A brand-new community — one with no summary at all — is regenerated
/// EAGERLY whatever its size. Laziness means "keep serving the old summary",
/// and there is no old summary; the alternative is a community that a global
/// search finds and cannot describe.
///
/// The 21-clique is deliberately ABOVE `STALE_USABLE_MIN_MEMBERS`, so this
/// fails if the size rule is applied without the has-summary override.
#[tokio::test]
async fn a_brand_new_community_is_summarised_eagerly_however_large_it_is() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();

    let n = (STALE_USABLE_MIN_MEMBERS + 1) as usize;
    let big = names("b", n);
    let order: Vec<&str> = big.iter().map(String::as_str).collect();
    seed_graph(&pool, ws, pg, &run, "big", &order, &clique(&big)).await;

    let provider = Counting::new(format!("sum-new-{run}"));
    CommunityWorker::new(pool.clone(), ws)
        .cold_run()
        .await
        .unwrap();

    let members: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM community_members cm
         JOIN communities c ON c.id = cm.community_id WHERE c.workspace_id = $1",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        members >= STALE_USABLE_MIN_MEMBERS,
        "sanity: the fixture must be on the LARGE side of the boundary ({members} members), or \
         this proves nothing about the has-summary override"
    );

    let pass = SummaryWorker::new(pool.clone(), provider.clone(), ws)
        .run_once()
        .await
        .unwrap();
    assert_eq!(
        (pass.regenerated, pass.marked_stale),
        (1, 0),
        "a community with no summary must be generated now, not marked stale_usable — there is \
         nothing to be stale WITH"
    );
    assert_eq!(provider.calls(), 1);
}

/// THE SMALL SIDE OF THE BOUNDARY. A membership change to a community below
/// `STALE_USABLE_MIN_MEMBERS` is proportionally large — one entity joining a
/// 4-member community changes a fifth of what the prose is about — so the old
/// summary is regenerated on the spot rather than served.
///
/// Note what produces the staleness: the HOT path
/// (`community::reassign_entity`) updates `member_set_hash` on the SURVIVING
/// row, so the summary row lives on carrying the hash it was generated for.
/// That difference is the entire staleness signal, and a cold-run swap could
/// not produce it — a changed membership deletes the old row and cascades its
/// summary away.
#[tokio::test]
async fn a_small_communitys_membership_change_is_regenerated_eagerly() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();
    seed_graph(&pool, ws, pg, &run, "g", TWO_CLIQUE_NODES, TWO_CLIQUES).await;

    let provider = Counting::new(format!("sum-small-{run}"));
    let worker = SummaryWorker::new(pool.clone(), provider.clone(), ws);
    let communities = CommunityWorker::new(pool.clone(), ws);
    communities.cold_run().await.unwrap();
    worker.run_once().await.unwrap();
    assert_eq!(provider.calls(), 2, "sanity: both communities summarised");

    // One new entity, strongly linked into the x clique. The hot path moves it
    // there, taking that community to 5 members and changing its hash in place.
    let new_edges: &[(&str, &str, f32)] = &[("newcomer", "xd", 5.0)];
    let ids = seed_graph(&pool, ws, pg, &run, "new", &["newcomer"], new_edges).await;
    let moved = communities.hot_reassign(&[ids["newcomer"]]).await.unwrap();
    assert_eq!(
        moved, 1,
        "sanity: the hot path must have placed the newcomer"
    );

    let pass = worker.run_once().await.unwrap();
    assert_eq!(
        (pass.regenerated, pass.marked_stale, pass.failed),
        (1, 0, 0),
        "the changed community is below the boundary and must be regenerated NOW; the unchanged \
         one must not be touched at all"
    );
    assert_eq!(
        provider.calls(),
        3,
        "EXACTLY one further model call — one community changed, not both"
    );

    let stored = stored(&pool, ws).await;
    let key = vec![
        "newcomer".to_string(),
        "xa".to_string(),
        "xb".to_string(),
        "xc".to_string(),
        "xd".to_string(),
    ];
    let (summary, state, _) = stored
        .get(&key)
        .unwrap_or_else(|| panic!("expected a summary for {key:?}; got {:?}", stored.keys()));
    assert_eq!(state, STATE_VALID);
    assert!(
        summary.contains("newcomer"),
        "the regenerated summary must describe the CURRENT membership: {summary:?}"
    );
}

/// THE LARGE SIDE OF THE BOUNDARY, and the lazy path end to end.
///
/// A community at or above `STALE_USABLE_MIN_MEMBERS` whose membership changed
/// keeps serving its existing prose, marked `stale_usable`, and costs **exactly
/// zero** model calls in that pass. "A slightly stale summary is far better
/// than a missing one" (design §2.2) — one entity joining a 21-member cluster
/// changes under 5% of what the summary is about, the same tolerance the cold
/// path applies to the partition as a whole.
///
/// Then `refresh` — what a global search calls when it touches a stale summary
/// — regenerates exactly that one community and returns it to `valid`.
#[tokio::test]
async fn a_large_communitys_membership_change_serves_a_stale_summary_until_refreshed() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();

    let n = STALE_USABLE_MIN_MEMBERS as usize;
    let big = names("b", n);
    let order: Vec<&str> = big.iter().map(String::as_str).collect();
    seed_graph(&pool, ws, pg, &run, "big", &order, &clique(&big)).await;

    let provider = Counting::new(format!("sum-large-{run}"));
    let worker = SummaryWorker::new(pool.clone(), provider.clone(), ws);
    let communities = CommunityWorker::new(pool.clone(), ws);
    communities.cold_run().await.unwrap();
    assert_eq!(worker.run_once().await.unwrap().regenerated, 1);
    let original = stored(&pool, ws)
        .await
        .into_values()
        .next()
        .map(|(s, _, _)| s)
        .unwrap();
    assert_eq!(provider.calls(), 1);

    // One newcomer joins, taking the community over the boundary and changing
    // its hash in place.
    let new_edges: &[(&str, &str, f32)] = &[("newcomer", big[0].as_str(), 5.0)];
    let ids = seed_graph(&pool, ws, pg, &run, "new", &["newcomer"], new_edges).await;
    assert_eq!(
        communities.hot_reassign(&[ids["newcomer"]]).await.unwrap(),
        1,
        "sanity: the hot path must have placed the newcomer"
    );

    let cid = community_holding(&pool, ws, "newcomer").await;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM community_members WHERE community_id = $1")
            .bind(cid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        count > STALE_USABLE_MIN_MEMBERS,
        "sanity: the changed community must be on the LARGE side ({count} members)"
    );

    let pass = worker.run_once().await.unwrap();
    assert_eq!(
        (pass.regenerated, pass.marked_stale, pass.failed),
        (0, 1, 0),
        "a large community's membership change must be absorbed lazily"
    );
    assert_eq!(
        provider.calls(),
        1,
        "EXACTLY zero further model calls — that is what 'lazy' has to mean to be worth anything"
    );

    let (served, state, _) = stored(&pool, ws).await.into_values().next().unwrap();
    assert_eq!(
        state, STATE_STALE_USABLE,
        "the summary must be MARKED stale, so a reader knows it is approximate"
    );
    assert_eq!(
        served, original,
        "and it must still be SERVING the old prose — a stale summary that was blanked is a \
         missing summary with extra steps"
    );

    // The lazy trigger: a reader touches it and it is brought up to date.
    assert!(
        worker.refresh(cid).await.unwrap(),
        "refresh must report that it did the work"
    );
    assert_eq!(
        provider.calls(),
        2,
        "EXACTLY one model call — refresh regenerates one community, not the workspace"
    );
    let (fresh, state, _) = stored(&pool, ws).await.into_values().next().unwrap();
    assert_eq!(
        state, STATE_VALID,
        "a regenerated summary must return to valid, or it stays flagged approximate forever"
    );
    assert!(
        fresh.contains("newcomer"),
        "and must describe the current membership: {fresh:?}"
    );
}

/// A summariser change is a FULL regeneration (design §3), which is the stated
/// reason `community_summaries` is keyed by `community_id` alone while
/// `embeddings` is keyed `(content_hash, model_id)` so two models coexist.
///
/// Two mechanisms, and the fixture reaches both: the queue's
/// `s.model_id IS DISTINCT FROM $2` predicate (without it nothing is pending at
/// all) and `classify`'s `model_changed` override (without it the LARGE
/// community is merely marked stale and keeps serving the old model's prose).
/// Hence one small community and one large one in the same workspace.
#[tokio::test]
async fn changing_the_summariser_regenerates_every_community_eagerly() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();

    let big = names("b", (STALE_USABLE_MIN_MEMBERS + 2) as usize);
    let mut edges = clique(&big);
    let small = ["sa", "sb", "sc"];
    edges.extend_from_slice(&[
        ("sa", "sb", 1.0),
        ("sa", "sc", 1.0),
        ("sb", "sc", 1.0),
        // A single weak bridge, so the two stay distinct communities.
        ("sa", big[0].as_str(), 0.1),
    ]);
    let mut order: Vec<&str> = big.iter().map(String::as_str).collect();
    order.extend_from_slice(&small);
    seed_graph(&pool, ws, pg, &run, "mixed", &order, &edges).await;

    CommunityWorker::new(pool.clone(), ws)
        .cold_run()
        .await
        .unwrap();

    let old = Counting::new(format!("sum-old-{run}"));
    let first = SummaryWorker::new(pool.clone(), old.clone(), ws)
        .run_once()
        .await
        .unwrap();
    assert_eq!(
        (first.regenerated, first.marked_stale),
        (2, 0),
        "sanity: the fixture must be two communities, one either side of the boundary"
    );
    let sizes: Vec<i64> = sqlx::query_scalar(
        "SELECT count(*) FROM community_members cm
         JOIN communities c ON c.id = cm.community_id
         WHERE c.workspace_id = $1 GROUP BY cm.community_id ORDER BY 1",
    )
    .bind(ws)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        sizes.len() == 2
            && sizes[0] < STALE_USABLE_MIN_MEMBERS
            && sizes[1] >= STALE_USABLE_MIN_MEMBERS,
        "sanity: one community below the boundary and one at or above it; got {sizes:?}"
    );

    let new = Counting::new(format!("sum-new-{run}"));
    let second = SummaryWorker::new(pool.clone(), new.clone(), ws)
        .run_once()
        .await
        .unwrap();
    assert_eq!(
        (second.regenerated, second.marked_stale, second.failed),
        (2, 0, 0),
        "every community must be regenerated under the new model — including the large one, which \
         the size rule alone would have marked stale_usable and left carrying the OLD model's prose"
    );
    assert_eq!(new.calls(), 2);
    assert_eq!(
        old.calls(),
        2,
        "and the old provider must not have been called again"
    );

    for (_, (_, state, model_id)) in stored(&pool, ws).await {
        assert_eq!(state, STATE_VALID);
        assert_eq!(
            model_id,
            new.model_id(),
            "every stored summary must record the model that actually wrote it"
        );
    }
}

// --------------------------------------------------------------- correctness --

/// A summary must describe ITS OWN community and no other.
///
/// The mechanism is the facts query's `WHERE cm.community_id = $1`. Deleting it
/// hands the summariser every member of the workspace, so both communities get
/// prose about all eight entities — which no assertion could catch under a
/// constant stub, and which is exactly why `StubSummariser` varies with
/// membership.
#[tokio::test]
async fn each_summary_describes_exactly_its_own_members() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();
    seed_graph(&pool, ws, pg, &run, "g", TWO_CLIQUE_NODES, TWO_CLIQUES).await;

    let provider = Counting::new(format!("sum-own-{run}"));
    CommunityWorker::new(pool.clone(), ws)
        .cold_run()
        .await
        .unwrap();
    SummaryWorker::new(pool.clone(), provider, ws)
        .run_once()
        .await
        .unwrap();

    let stored = stored(&pool, ws).await;
    assert_eq!(stored.len(), 2, "sanity: two communities, two summaries");
    for (members, (summary, _, _)) in &stored {
        for m in members {
            assert!(
                summary.contains(m.as_str()),
                "the summary of {members:?} must mention its member {m}: {summary:?}"
            );
        }
        // The names of every entity in the OTHER community. Compared as whole
        // names, not by prefix letter: the stub's prose contains plenty of
        // ordinary English, so a substring test on "a" would fire on the word
        // "A level 0 community" and pass for the wrong reason.
        for foreign in stored.keys().filter(|k| *k != members).flatten() {
            assert!(
                !summary.contains(foreign.as_str()),
                "the summary of {members:?} must not mention {foreign}, which belongs to the \
                 other community: {summary:?}"
            );
        }
    }
}

/// Members reach the summariser in canonical `entities.name` order, so the same
/// membership always produces the same prompt — and, under a deterministic
/// provider, the same prose.
///
/// Two workspaces build the SAME graph in two different entity-creation orders,
/// so their `gen_random_uuid()` ids are two independent random permutations.
/// The summaries must be identical. The mechanism is `ORDER BY e.name ASC` in
/// the facts query; ordering by `id` (or not ordering at all) makes these two
/// disagree, which would surface much later as a deterministic pipeline that
/// looks non-deterministic.
#[tokio::test]
async fn members_reach_the_summariser_in_canonical_name_order() {
    let pool = connect().await;
    let run = Uuid::new_v4().simple().to_string();

    let base: Vec<&str> = TWO_CLIQUE_NODES.to_vec();
    let mut reversed = base.clone();
    reversed.reverse();

    let mut summaries = Vec::new();
    for (i, order) in [base, reversed].into_iter().enumerate() {
        let ws = workspace(&pool).await;
        let pg = page(&pool, ws).await;
        seed_graph(&pool, ws, pg, &run, &format!("ord{i}"), &order, TWO_CLIQUES).await;
        let provider = Counting::new(format!("sum-order-{run}"));
        CommunityWorker::new(pool.clone(), ws)
            .cold_run()
            .await
            .unwrap();
        SummaryWorker::new(pool.clone(), provider, ws)
            .run_once()
            .await
            .unwrap();

        let mut texts: Vec<(Vec<String>, String)> = stored(&pool, ws)
            .await
            .into_iter()
            .map(|(k, (s, _, _))| (k, s))
            .collect();
        texts.sort();
        summaries.push(texts);
    }

    assert_eq!(
        summaries[0].len(),
        2,
        "sanity: two communities, or the equality below is close to vacuous"
    );
    assert_eq!(
        summaries[0], summaries[1],
        "two workspaces holding the same graph under different entity-id orders must produce \
         byte-identical summaries; member order is leaking entity ids into the prompt"
    );
}

/// A summary is attributed to the membership it ACTUALLY describes — the
/// members that were read and handed to the model — never to whatever
/// `communities.member_set_hash` happens to say at the moment of the write.
///
/// The two can differ: the hot path can commit a move between the facts read
/// and the insert. Copying the row's hash would then stamp the summary as
/// describing a membership it does not describe, and because the queue is a set
/// difference that compares exactly those two hashes, the community would never
/// be revisited. Permanent and silent — the error direction M2a's 0008 backfill
/// ruled out. Recomputing can only err the other way: the community stays
/// queued and costs one redundant model call.
///
/// The interleaving is REPRESENTED rather than raced. Injecting a commit
/// between two statements of a private method is not reproducible without a
/// test-only seam, so the state it produces is built directly: the community
/// row's hash is doctored to disagree with its own members, exactly as a
/// concurrent move would leave it from this write's point of view. Without
/// this test the mechanism survives its own deletion — measured, which is why
/// it exists.
#[tokio::test]
async fn a_summary_records_the_membership_it_was_generated_for_not_the_rows_claim() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();
    seed_graph(&pool, ws, pg, &run, "g", TWO_CLIQUE_NODES, TWO_CLIQUES).await;

    let provider = Counting::new(format!("sum-hash-{run}"));
    let worker = SummaryWorker::new(pool.clone(), provider.clone(), ws);
    CommunityWorker::new(pool.clone(), ws)
        .cold_run()
        .await
        .unwrap();
    worker.run_once().await.unwrap();
    assert_eq!(provider.calls(), 2, "sanity: both communities summarised");

    // The community row now claims a membership it does not have — what a
    // concurrent hot-path move looks like to a write already in flight.
    let cid = community_holding(&pool, ws, "aa").await;
    let doctored = "0".repeat(64);
    sqlx::query("UPDATE communities SET member_set_hash = $2 WHERE id = $1")
        .bind(cid)
        .bind(&doctored)
        .execute(&pool)
        .await
        .unwrap();

    let pass = worker.run_once().await.unwrap();
    assert_eq!(
        (pass.regenerated, pass.marked_stale),
        (1, 0),
        "sanity: the doctored community must look stale and be regenerated"
    );

    let stamped: String = sqlx::query_scalar(
        "SELECT member_set_hash FROM community_summaries WHERE community_id = $1",
    )
    .bind(cid)
    .fetch_one(&pool)
    .await
    .unwrap();
    let members: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM community_members WHERE community_id = $1")
            .bind(cid)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_ne!(
        stamped, doctored,
        "the summary must NOT be stamped with the community row's claim — doing so would mark it \
         current for a membership it never saw, and the set-difference queue would never revisit it"
    );
    assert_eq!(
        stamped,
        noted_db::community::member_set_hash(&members),
        "it must be stamped with the hash of the members that were actually summarised"
    );

    assert_eq!(
        pending_summaries(&pool, ws, provider.model_id())
            .await
            .unwrap()
            .len(),
        1,
        "and the community must STAY queued while the row's claim disagrees — under-marking, \
         which self-corrects, rather than over-marking, which is permanent"
    );
}

// ------------------------------------------------------- failure isolation --

/// One community the model cannot summarise must not deny every other
/// community its summary. Same discipline as `extract_worker`'s poison chunk:
/// logged, counted, skipped, and LEFT IN THE QUEUE — the set-difference query
/// re-offers it on the next pass, which is the only retry mechanism a queue
/// with no state needs.
#[tokio::test]
async fn one_communitys_summariser_failure_does_not_stop_the_pass() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();

    // Two triangles joined weakly; one of them contains a `poison` entity.
    let edges: &[(&str, &str, f32)] = &[
        ("aa", "ab", 1.0),
        ("aa", "ac", 1.0),
        ("ab", "ac", 1.0),
        ("poison1", "poison2", 1.0),
        ("poison1", "poison3", 1.0),
        ("poison2", "poison3", 1.0),
        ("ac", "poison1", 0.1),
    ];
    seed_graph(
        &pool,
        ws,
        pg,
        &run,
        "poison",
        &["aa", "ab", "ac", "poison1", "poison2", "poison3"],
        edges,
    )
    .await;

    let provider = Arc::new(FailsOnPoison(AtomicUsize::new(0)));
    CommunityWorker::new(pool.clone(), ws)
        .cold_run()
        .await
        .unwrap();
    let worker = SummaryWorker::new(pool.clone(), provider.clone(), ws);

    let pass = worker.run_once().await.unwrap();
    assert_eq!(
        (pass.regenerated, pass.failed),
        (1, 1),
        "the healthy community must be summarised and the failing one counted, not propagated"
    );
    assert_eq!(
        provider.0.load(Ordering::SeqCst),
        2,
        "both communities must have been attempted; a pass that stopped at the failure would \
         show 1"
    );

    let stored = stored(&pool, ws).await;
    assert_eq!(
        stored.len(),
        1,
        "the failing community must have NO summary row rather than a placeholder one"
    );
    assert!(
        stored.keys().all(|k| !k[0].starts_with("poison")),
        "and the stored one must be the healthy community: {:?}",
        stored.keys()
    );

    let still_pending = pending_summaries(&pool, ws, provider.model_id())
        .await
        .unwrap();
    assert_eq!(
        still_pending.len(),
        1,
        "the failing community must stay in the queue for the next pass — there is nowhere else \
         for it to go"
    );
}

/// A model that "succeeds" with nothing in it is a failure that has not
/// noticed. Storing the empty string would satisfy the set-difference queue
/// forever after, leaving the community permanently summarised with nothing —
/// permanent and silent, the error direction M2a's 0008 backfill ruled out.
#[tokio::test]
async fn an_empty_summary_is_never_stored() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let pg = page(&pool, ws).await;
    let run = Uuid::new_v4().simple().to_string();
    seed_graph(&pool, ws, pg, &run, "g", TWO_CLIQUE_NODES, TWO_CLIQUES).await;

    let provider = Arc::new(EmptyOutput);
    CommunityWorker::new(pool.clone(), ws)
        .cold_run()
        .await
        .unwrap();
    let worker = SummaryWorker::new(pool.clone(), provider.clone(), ws);

    let pass = worker.run_once().await.unwrap();
    assert_eq!(
        (pass.regenerated, pass.failed),
        (0, 2),
        "an empty summary must be treated exactly as a model failure"
    );
    assert!(
        stored(&pool, ws).await.is_empty(),
        "nothing may be stored — a stored empty summary is permanent and silent, because the \
         queue would then consider the community done forever"
    );
    assert_eq!(
        pending_summaries(&pool, ws, provider.model_id())
            .await
            .unwrap()
            .len(),
        2,
        "both communities must remain queued for a retry"
    );
}

// ------------------------------------------------------------------ tenancy --

/// Summarising workspace A must not read, rewrite, or re-flag anything in
/// workspace B — not merely "not corrupt it", but leave B's rows byte-identical.
///
/// Both halves are asserted because they die to different mutations: A's
/// summaries must contain none of B's entity names (a missing filter on the
/// READ), and B's `community_summaries` rows must be untouched (a missing
/// filter on the QUEUE, which would have A regenerate B's communities under A's
/// model id).
#[tokio::test]
async fn summarising_one_workspace_cannot_touch_another() {
    let pool = connect().await;
    let ws_a = workspace(&pool).await;
    let ws_b = workspace(&pool).await;
    let page_a = page(&pool, ws_a).await;
    let page_b = page(&pool, ws_b).await;
    let run = Uuid::new_v4().simple().to_string();

    seed_graph(
        &pool,
        ws_a,
        page_a,
        &run,
        "a",
        TWO_CLIQUE_NODES,
        TWO_CLIQUES,
    )
    .await;
    let b_edges: &[(&str, &str, f32)] = &[
        ("bb1", "bb2", 1.0),
        ("bb1", "bb3", 1.0),
        ("bb2", "bb3", 1.0),
    ];
    seed_graph(
        &pool,
        ws_b,
        page_b,
        &run,
        "b",
        &["bb1", "bb2", "bb3"],
        b_edges,
    )
    .await;

    let b_provider = Counting::new(format!("sum-b-{run}"));
    CommunityWorker::new(pool.clone(), ws_b)
        .cold_run()
        .await
        .unwrap();
    SummaryWorker::new(pool.clone(), b_provider.clone(), ws_b)
        .run_once()
        .await
        .unwrap();
    let b_before: Vec<(Uuid, String, String, String, String)> = sqlx::query_as(
        "SELECT s.community_id, s.model_id, s.summary, s.state, s.member_set_hash
         FROM community_summaries s
         JOIN communities c ON c.id = s.community_id
         WHERE c.workspace_id = $1 ORDER BY s.community_id",
    )
    .bind(ws_b)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        b_before.len(),
        1,
        "sanity: B must have a real summary to be damaged"
    );

    let a_provider = Counting::new(format!("sum-a-{run}"));
    CommunityWorker::new(pool.clone(), ws_a)
        .cold_run()
        .await
        .unwrap();
    let pass = SummaryWorker::new(pool.clone(), a_provider.clone(), ws_a)
        .run_once()
        .await
        .unwrap();
    assert_eq!(
        (pass.regenerated, pass.marked_stale),
        (2, 0),
        "A must summarise A's two communities and ONLY those — B's is live in the same database"
    );
    assert_eq!(a_provider.calls(), 2);

    for (members, (summary, _, _)) in stored(&pool, ws_a).await {
        assert!(
            members.iter().all(|m| !m.starts_with("bb")) && !summary.contains("bb"),
            "no entity of B's may appear in A's partition or its prose; got {members:?} / \
             {summary:?}"
        );
    }

    let b_after: Vec<(Uuid, String, String, String, String)> = sqlx::query_as(
        "SELECT s.community_id, s.model_id, s.summary, s.state, s.member_set_hash
         FROM community_summaries s
         JOIN communities c ON c.id = s.community_id
         WHERE c.workspace_id = $1 ORDER BY s.community_id",
    )
    .bind(ws_b)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        b_after, b_before,
        "B's summary rows — model, prose, state and hash — must be byte-identical after A ran"
    );
}

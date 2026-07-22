//! M2c Task 2 — `AnswerProvider` + `graph_search::local_search` end to end.
//!
//! Task 1 proved the RETRIEVAL (in `noted-db`). These tests prove the WIRING:
//! that hybrid's pages become the graph's seed chunks with their ranks intact,
//! that `GraphHit::hops` survives all the way to a citation and to the prompt,
//! that an answerer is not called when there is nothing to answer from, and that
//! an empty answer is refused rather than presented.
//!
//! Every fixture gets its OWN workspace, and every `model_id` is unique to its
//! test. Tests share a dev database with other agents' fixtures, so nothing here
//! may assert anything instance-wide — `materialize.rs` carries the scar from
//! the last time something did.
use noted_index::answer::{
    AnswerError, AnswerProvider, AnswerRequest, ContextItem, StubAnswerer, hop_note, verify_answer,
};
use noted_index::graph_search::{Inclusion, LocalAnswer, LocalSearchError, local_search};
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

// ---------------------------------------------------------------- fixtures --

/// A fresh, never-before-used `model_id` per test — the same helper and the same
/// rationale as `hybrid.rs`, `related.rs` and `noted-db`'s `graph_search.rs`:
/// `embeddings_hnsw_idx` is APPROXIMATE and its recall degrades with the vector
/// count under one `model_id`, so a unique id keeps each test's vector space
/// small enough that ANN search is exact for it.
fn unique_model() -> String {
    format!("ls-model-{}", Uuid::new_v4())
}

async fn connect() -> noted_db::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

/// A user for the ACL filter. These fixtures set no overrides, so the filter is
/// a pass-through — `noted-db`'s `tests/acl.rs` proves the denial behaviour.
async fn acl_user(pool: &noted_db::PgPool) -> Uuid {
    let email = format!("ls{}@example.com", Uuid::new_v4().simple());
    noted_db::users::create(pool, &email, "hash", "T").await.unwrap().id
}

async fn workspace(pool: &noted_db::PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('local-search-test') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A live page with one block (so FTS can see it) and one chunk.
async fn page(pool: &noted_db::PgPool, ws: Uuid, title: &str, text: &str) -> (Uuid, String) {
    let id: Uuid =
        sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1, $2) RETURNING id")
            .bind(ws)
            .bind(title)
            .fetch_one(pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
         VALUES ($1, 0, 'paragraph', $2, md5($2))",
    )
    .bind(id)
    .bind(text)
    .execute(pool)
    .await
    .unwrap();
    let hash = format!("ls-{}", Uuid::new_v4());
    noted_db::chunks::upsert(pool, &[(hash.clone(), text.to_string(), 10)])
        .await
        .unwrap();
    noted_db::chunks::set_page_chunks(pool, id, &[hash.clone()])
        .await
        .unwrap();
    (id, hash)
}

/// As `page`, plus an embedding on one axis of the 768-dim space, so the page is
/// reachable by hybrid's vector arm at a controlled distance.
async fn embedded_page(
    pool: &noted_db::PgPool,
    ws: Uuid,
    title: &str,
    text: &str,
    axis: usize,
    model: &str,
) -> (Uuid, String) {
    let (id, hash) = page(pool, ws, title, text).await;
    noted_db::chunks::store_embedding(pool, &hash, model, &vec_at(axis))
        .await
        .unwrap();
    (id, hash)
}

fn vec_at(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; 768];
    v[axis] = 1.0;
    v
}

/// A uniquely-named entity — unique because `entities` is `UNIQUE (workspace_id,
/// name)` and fixtures share a database. The `label` prefix keeps the ORDER BY
/// name that `seed_entities` promises predictable within one test.
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

/// One edge, extracted from `chunk`, through the same production writer the
/// extraction worker uses — a hand-rolled INSERT could set a `workspace_id` no
/// writer ever sets.
async fn edge(pool: &noted_db::PgPool, ws: Uuid, chunk: &str, model: &str, a: Uuid, b: Uuid) {
    noted_db::graph::replace_chunk_edges(
        pool,
        ws,
        chunk,
        model,
        &[(a, b, "relates_to".to_string(), 1.0)],
    )
    .await
    .unwrap();
}

fn cited(ans: &LocalAnswer, page_id: Uuid) -> Option<&noted_index::graph_search::Citation> {
    ans.citations.iter().find(|c| c.page_id == page_id)
}

// ---------------------------------------------------------------- providers --

/// Counts calls, delegates to the stub. `==` assertions on `calls()` are the
/// zero-call discipline this project requires; every such assertion in this file
/// is preceded by one that proves the provider ran at all, so it cannot pass
/// because nothing happened.
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

/// "Succeeds" with nothing in it — a real model that ran out of context,
/// refused, or emitted only a fence that got stripped. The stub deliberately
/// never fails (a stub that failed on a magic question would be a trap of its
/// own), so the failure paths get purpose-built providers, the same shape
/// `tests/summary.rs` uses.
struct EmptyOutput;

#[async_trait::async_trait]
impl AnswerProvider for EmptyOutput {
    fn model_id(&self) -> &str {
        "empty-answerer"
    }
    async fn synthesise(&self, _req: &AnswerRequest) -> Result<String, AnswerError> {
        Ok("  \n\t ".into())
    }
}

/// Records the last request it saw, so a test can assert on the PROMPT rather
/// than only on the prose a stub happened to build out of it.
struct Recording(std::sync::Mutex<Option<AnswerRequest>>);

#[async_trait::async_trait]
impl AnswerProvider for Recording {
    fn model_id(&self) -> &str {
        "recording-answerer"
    }
    async fn synthesise(&self, req: &AnswerRequest) -> Result<String, AnswerError> {
        *self.0.lock().unwrap() = Some(req.clone());
        Ok("recorded".into())
    }
}

// ------------------------------------------------------------------- tests --

/// The end-to-end shape of the surface, and the one that carries the product
/// claim: a page the graph reached is cited, and the citation SAYS SO.
///
/// Fixture: ALPHA carries the question's terms and sits on the query vector.
/// BETA shares no term with the question and is embedded far away — hybrid at
/// this `k` does not return it (asserted, so the premise cannot rot). The only
/// thing tying BETA to the question is one edge: ALPHA's chunk names an incident
/// and a person, BETA's chunk names that person and a hobby. Five decoys sit ON
/// the query axis so that a five-page workspace cannot hand hybrid the answer by
/// accident — the same fixture reasoning as Task 1's crown jewel.
///
/// A FOREIGN workspace holds a page with ALPHA's exact text. It must not be
/// cited: `seeds_from_pages` names both a page and a chunk and carries no
/// workspace predicate of its own (its guard is upstream, in `hybrid`), and this
/// is where that claim is checked rather than asserted in a comment.
///
/// MECHANISMS PROTECTED:
///   * `Inclusion::from_hops(h.hops)` — the citation's "why". Pinned to
///     `Inclusion::Seed`, the BETA assertion fails.
///   * `hop_note` reaching the prompt — the answer text names the hop.
///   * `subjects:` on the request — blanked, the stub says "no named subject".
///   * `workspace_id` reaching `hybrid` — the foreign page appears.
#[tokio::test]
async fn a_graph_reached_page_is_cited_and_the_citation_says_why() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let foreign = workspace(&pool).await;
    let model = unique_model();

    let question = "ECONNREFUSED deployment incident";
    let q_vec = vec_at(700);
    let alpha_text = "the ECONNREFUSED deployment incident was diagnosed on friday";

    let (alpha, alpha_chunk) =
        embedded_page(&pool, ws, "Postmortem", alpha_text, 700, &model).await;
    let (beta, beta_chunk) = embedded_page(
        &pool,
        ws,
        "Weekend",
        "sourdough starter needs feeding twice daily in warm weather",
        500,
        &model,
    )
    .await;
    for i in 0..5 {
        embedded_page(
            &pool,
            ws,
            &format!("Decoy{i}"),
            &format!("miscellaneous jottings number {i} with nothing in common"),
            700,
            &model,
        )
        .await;
    }
    // Same text, different tenant.
    let (intruder, _) = embedded_page(&pool, foreign, "Postmortem", alpha_text, 700, &model).await;

    let incident = entity(&pool, ws, "a-incident").await;
    let person = entity(&pool, ws, "b-person").await;
    let hobby = entity(&pool, ws, "c-hobby").await;
    edge(&pool, ws, &alpha_chunk, &model, incident, person).await;
    edge(&pool, ws, &beta_chunk, &model, person, hobby).await;

    // PREMISE: at this k, plain hybrid does not return BETA. Without this the
    // test could pass on a page the graph never had to reach.
    let plain = noted_db::search::hybrid(&pool, ws, acl_user(&pool).await, question, &q_vec, &model, 6)
        .await
        .unwrap();
    assert!(
        plain.iter().any(|h| h.page_id == alpha),
        "fixture broken: hybrid must find the question's own subject"
    );
    assert!(
        !plain.iter().any(|h| h.page_id == beta),
        "PREMISE: hybrid alone must NOT return the graph-only page"
    );

    let provider = Counting::new();
    let ans = local_search(&pool, ws, acl_user(&pool).await, question, &q_vec, &model, &provider, 6)
        .await
        .unwrap();

    // Sanity before any `==` count assertion.
    assert!(!ans.citations.is_empty(), "expected evidence");
    assert_eq!(provider.calls(), 1, "exactly one synthesis call per search");

    let alpha_cite = cited(&ans, alpha).expect("the seed page must be cited");
    assert_eq!(
        alpha_cite.why,
        Inclusion::Seed,
        "hybrid found ALPHA itself, so its citation must say so"
    );
    assert!(!alpha_cite.content_hash.is_empty());

    let beta_cite = cited(&ans, beta).expect(
        "THE POINT: the graph-reached page must be cited even though hybrid ranked it out",
    );
    assert_eq!(
        beta_cite.why,
        Inclusion::Graph { hops: 1 },
        "BETA is one edge from the question's subject and the citation must say which"
    );

    assert!(
        cited(&ans, intruder).is_none(),
        "another workspace's identical page must never be cited"
    );

    // The "why these results" surface, and the prompt built from it.
    let names: Vec<&str> = ans.seed_entities.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.iter().any(|n| n.starts_with("a-incident")),
        "seed entities must name what the question turned out to be about, got {names:?}"
    );
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "seed entities arrive ordered by name");

    for n in &names {
        assert!(
            ans.answer.contains(n),
            "the seed entities must reach the prompt: {n} missing from {:?}",
            ans.answer
        );
    }
    assert!(
        ans.answer.contains(&hop_note(1)),
        "the answer must be able to say a passage came from the graph: {:?}",
        ans.answer
    );
    assert!(
        ans.answer.contains(&hop_note(0)),
        "...and which passages hybrid found directly: {:?}",
        ans.answer
    );
}

/// Hybrid's ORDERING must survive into the graph's ranking.
///
/// A seed chunk's rank is its page's 1-based position in the fused hybrid
/// result, because that is the quantity `1/(RRF_K + seed_rank)` expects. FIRST
/// carries the question's terms AND sits on the query vector; SECOND has neither
/// and is orthogonal, so hybrid's order is forced rather than incidental — and
/// asserted, so the premise cannot rot.
///
/// Both hits are seeds at equal hops and equal weight, so the only thing
/// separating their scores is rank: 1/61 > 1/62, a STRICT inequality. (Deleting
/// the mechanism outright — a constant rank for every page — produces an exact
/// TIE broken by a random `page_id`, and per this project's standing rule a
/// ranking mutation that produces a tie is not evidence. The mutation that IS
/// evidence is the plausible bug: enumerate the pages in the wrong direction.)
///
/// MECHANISM PROTECTED: `rank: i as i32 + 1` in `seeds_from_pages`. Reversed,
/// this test fails.
#[tokio::test]
async fn citation_order_follows_hybrid_rank() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let model = unique_model();

    let question = "quarterly budget forecast";
    let q_vec = vec_at(300);

    let (first, _) = embedded_page(
        &pool,
        ws,
        "Budget",
        "the quarterly budget forecast was revised upward",
        300,
        &model,
    )
    .await;
    let (second, _) = embedded_page(
        &pool,
        ws,
        "Kitchen",
        "replace the tap washer before the weekend",
        301,
        &model,
    )
    .await;

    let plain = noted_db::search::hybrid(&pool, ws, acl_user(&pool).await, question, &q_vec, &model, 10)
        .await
        .unwrap();
    let ids: Vec<Uuid> = plain.iter().map(|h| h.page_id).collect();
    assert_eq!(
        ids,
        vec![first, second],
        "PREMISE: hybrid must rank FIRST above SECOND, or this test measures nothing"
    );

    let ans = local_search(&pool, ws, acl_user(&pool).await, question, &q_vec, &model, &StubAnswerer::new(), 10)
        .await
        .unwrap();

    let order: Vec<Uuid> = ans.citations.iter().map(|c| c.page_id).collect();
    assert_eq!(
        order,
        vec![first, second],
        "citations must inherit hybrid's ordering through the seed ranks"
    );
}

/// No evidence means NO synthesis call.
///
/// A model handed a question and no context answers it from its weights, and a
/// fluent answer above an empty citation list is indistinguishable from a
/// well-sourced one. So local search declines to ask.
///
/// The `== 1` is guarded: the SAME provider is first used on a workspace that
/// does have evidence, so this cannot pass because nothing ran at all. The
/// second workspace is genuinely empty (no pages), which is what makes both
/// hybrid arms return nothing — note that hybrid's vector arm has NO distance
/// threshold, so in a workspace with embeddings under this model it would return
/// the nearest chunks however irrelevant, and rigging the emptiness any other
/// way would rig it upstream of the mechanism.
///
/// MECHANISM PROTECTED: the `if citations.is_empty()` early return in
/// `local_search`. Deleted, the call count is 2.
#[tokio::test]
async fn no_evidence_means_no_provider_call() {
    let pool = connect().await;
    let stocked = workspace(&pool).await;
    let empty = workspace(&pool).await;
    let model = unique_model();

    let question = "migration rollback plan";
    let q_vec = vec_at(120);
    embedded_page(
        &pool,
        stocked,
        "Runbook",
        "the migration rollback plan is rehearsed each release",
        120,
        &model,
    )
    .await;

    let provider = Counting::new();

    let found = local_search(&pool, stocked, acl_user(&pool).await, question, &q_vec, &model, &provider, 5)
        .await
        .unwrap();
    assert!(
        !found.citations.is_empty(),
        "SANITY: the provider must have had something to answer from"
    );
    assert_eq!(provider.calls(), 1);

    let none = local_search(&pool, empty, acl_user(&pool).await, question, &q_vec, &model, &provider, 5)
        .await
        .unwrap();
    assert!(none.citations.is_empty());
    assert!(none.seed_entities.is_empty());
    assert_eq!(
        provider.calls(),
        1,
        "an answerer must NOT be asked to answer from nothing"
    );
    assert!(
        none.answer.contains("migration rollback plan"),
        "the empty result still has to say what was asked: {:?}",
        none.answer
    );
}

/// An empty answer is refused, not presented.
///
/// `community_summaries.summary` taught this: a model that returns `""` on
/// refusal or context exhaustion produces a response that LOOKS successful. Here
/// it would render as a citation list with nothing above it.
///
/// The failure is rigged AT the mechanism — the provider itself returns
/// whitespace — so nothing upstream can make this pass by early return.
///
/// MECHANISM PROTECTED: the `verify_answer` call in `local_search`. Deleted,
/// this returns `Ok("  \n\t ")`.
#[tokio::test]
async fn an_empty_answer_is_refused_rather_than_presented() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let model = unique_model();

    let question = "oncall handover notes";
    let q_vec = vec_at(140);
    embedded_page(
        &pool,
        ws,
        "Handover",
        "oncall handover notes go in the shared channel",
        140,
        &model,
    )
    .await;

    // SANITY: this fixture does produce evidence, so the provider IS reached.
    let ok = local_search(&pool, ws, acl_user(&pool).await, question, &q_vec, &model, &StubAnswerer::new(), 5)
        .await
        .unwrap();
    assert!(!ok.citations.is_empty());

    let err = local_search(&pool, ws, acl_user(&pool).await, question, &q_vec, &model, &EmptyOutput, 5)
        .await
        .expect_err("an empty answer must not be presented as an answer");
    match err {
        LocalSearchError::Answer(AnswerError::Invalid(m)) => {
            assert!(m.contains("empty-answerer"), "{m}")
        }
        other => panic!("expected an Invalid answer error, got {other:?}"),
    }
}

/// `k` is clamped into `1..=MAX_LOCAL_LIMIT` before it reaches hybrid.
///
/// `local_search_chunks` clamps its own limit but `hybrid` does not, so an
/// unclamped `k = 0` becomes `LIMIT 0`, which returns no pages, which returns no
/// seeds, which produces a confident "nothing bears on this" over a workspace
/// that plainly does have the answer. That is a silent wrong answer, not an
/// error.
///
/// MECHANISM PROTECTED: `let k = k.clamp(1, MAX_LOCAL_LIMIT)`. Deleted, `k = 0`
/// yields zero citations.
#[tokio::test]
async fn a_zero_k_is_clamped_rather_than_returning_nothing() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let model = unique_model();

    let question = "espresso grinder settings";
    let q_vec = vec_at(160);
    embedded_page(
        &pool,
        ws,
        "Coffee",
        "espresso grinder settings drift as the burrs wear",
        160,
        &model,
    )
    .await;
    embedded_page(
        &pool,
        ws,
        "Coffee two",
        "espresso grinder settings depend on the roast date",
        161,
        &model,
    )
    .await;

    // SANITY: a normal k returns both, so the fixture has more than one page to
    // clamp down to.
    let plenty = local_search(&pool, ws, acl_user(&pool).await, question, &q_vec, &model, &StubAnswerer::new(), 5)
        .await
        .unwrap();
    assert_eq!(plenty.citations.len(), 2);

    let clamped = local_search(&pool, ws, acl_user(&pool).await, question, &q_vec, &model, &StubAnswerer::new(), 0)
        .await
        .unwrap();
    assert_eq!(
        clamped.citations.len(),
        1,
        "k = 0 must clamp to one result, not collapse to none"
    );
}

/// The prompt is built from the RETRIEVAL rows, and carries the provenance.
///
/// Asserted on the request itself rather than on prose a stub happened to build,
/// so a change to the stub's wording cannot quietly retire this. `source` is the
/// page title, `text` the chunk, `note` the hop explanation.
///
/// MECHANISM PROTECTED: the `context:` construction in `local_search` — in
/// particular `note: hop_note(c.why.hops())`. Pinned to a constant note, this
/// fails.
#[tokio::test]
async fn the_prompt_carries_the_provenance_of_every_passage() {
    let pool = connect().await;
    let ws = workspace(&pool).await;
    let model = unique_model();

    let question = "certificate rotation runbook";
    let q_vec = vec_at(180);
    let (alpha, alpha_chunk) = embedded_page(
        &pool,
        ws,
        "Certs",
        "the certificate rotation runbook lives beside the terraform",
        180,
        &model,
    )
    .await;
    let (beta, beta_chunk) = embedded_page(
        &pool,
        ws,
        "Gardening",
        "tomatoes want a deep soak twice a week",
        520,
        &model,
    )
    .await;

    // Decoys, and they are not padding: with only ALPHA and BETA in the
    // workspace, hybrid at k=6 returns BOTH, so BETA arrives as a SEED and the
    // hop note this test exists to check is never exercised. The premise is
    // asserted below rather than assumed — the same discipline
    // `a_graph_reached_page_is_cited_and_the_citation_says_why` uses, and its
    // absence here is exactly why this test once passed against a `note` that
    // was wrong.
    for i in 0..5 {
        embedded_page(
            &pool,
            ws,
            &format!("Decoy{i}"),
            &format!("unrelated filing number {i} about nothing in particular"),
            180,
            &model,
        )
        .await;
    }

    let a = entity(&pool, ws, "a-cert").await;
    let b = entity(&pool, ws, "b-owner").await;
    let c = entity(&pool, ws, "c-plot").await;
    edge(&pool, ws, &alpha_chunk, &model, a, b).await;
    edge(&pool, ws, &beta_chunk, &model, b, c).await;

    // PREMISE: BETA must be reachable ONLY through the graph, or the hop-note
    // assertion below is vacuous.
    let plain = noted_db::search::hybrid(&pool, ws, acl_user(&pool).await, question, &q_vec, &model, 6)
        .await
        .unwrap();
    assert!(
        plain.iter().any(|h| h.page_id == alpha),
        "fixture broken: hybrid must find the question's own subject"
    );
    assert!(
        !plain.iter().any(|h| h.page_id == beta),
        "PREMISE: hybrid alone must NOT return the graph-only page"
    );

    let provider = Recording(std::sync::Mutex::new(None));
    let ans = local_search(&pool, ws, acl_user(&pool).await, question, &q_vec, &model, &provider, 6)
        .await
        .unwrap();
    assert_eq!(ans.answer, "recorded");

    let req = provider.0.lock().unwrap().clone().expect("provider ran");
    assert_eq!(req.question, question);
    assert_eq!(
        req.context.len(),
        ans.citations.len(),
        "every citation must be in front of the answerer, and vice versa"
    );

    let alpha_item = req
        .context
        .iter()
        .find(|c| c.source == "Certs")
        .expect("the seed page must be in the prompt");
    assert_eq!(alpha_item.note, hop_note(0));
    assert!(alpha_item.text.contains("certificate rotation runbook"));

    let beta_item = req
        .context
        .iter()
        .find(|c| c.source == "Gardening")
        .expect("the graph-reached page must be in the prompt");
    assert_eq!(
        beta_item.note,
        hop_note(1),
        "a hop must be explained to the model, not just to the UI"
    );

    // ...and the citation for the same page agrees with the prompt line.
    assert_eq!(
        cited(&ans, alpha).unwrap().why,
        Inclusion::from_hops(0),
        "citation and prompt must be derived from one value"
    );
}

// ------------------------------------------------------- provider unit tests --

/// The stub must be DISCRIMINATING, not merely deterministic.
///
/// M2a's uniform `StubExtractor` left four production paths structurally
/// unreachable by any test that used it. So this asserts the stub's output is a
/// function of EVERY field of the request: change one, the string changes. A
/// stub that failed this would make every wiring bug between retrieval and
/// prompt invisible, and the tests above would pass over broken code.
#[tokio::test]
async fn the_stub_varies_with_every_field_of_its_request() {
    let s = StubAnswerer::new();
    let base = AnswerRequest {
        question: "what changed".into(),
        subjects: vec!["alpha".into()],
        context: vec![ContextItem {
            source: "Page A".into(),
            text: "the first passage".into(),
            note: hop_note(0),
        }],
    };
    let baseline = s.synthesise(&base).await.unwrap();

    let mut variants: Vec<(&str, AnswerRequest)> = Vec::new();

    let mut v = base.clone();
    v.question = "what else changed".into();
    variants.push(("question", v));

    let mut v = base.clone();
    v.subjects = vec!["beta".into()];
    variants.push(("which subject", v));

    let mut v = base.clone();
    v.subjects = vec!["alpha".into(), "beta".into()];
    variants.push(("how many subjects", v));

    let mut v = base.clone();
    v.subjects = vec!["beta".into(), "alpha".into()];
    variants.push(("subject ORDER", v));

    let mut v = base.clone();
    v.subjects.clear();
    variants.push(("no subjects at all", v));

    let mut v = base.clone();
    v.context[0].source = "Page B".into();
    variants.push(("which page", v));

    let mut v = base.clone();
    v.context[0].text = "an entirely different passage".into();
    variants.push(("passage text", v));

    let mut v = base.clone();
    v.context[0].note = hop_note(1);
    variants.push(("seed vs hop", v));

    let mut v = base.clone();
    v.context.push(ContextItem {
        source: "Page B".into(),
        text: "the second passage".into(),
        note: hop_note(2),
    });
    variants.push(("how many passages", v));

    let mut v = base.clone();
    v.context.clear();
    variants.push(("no evidence at all", v));

    for (axis, req) in &variants {
        let out = s.synthesise(req).await.unwrap();
        assert_ne!(
            out, baseline,
            "the stub must vary with {axis}; a constant stub hides wiring bugs"
        );
        assert!(!out.trim().is_empty(), "the stub never returns empty");
    }

    // Deterministic: the same request twice is the same string.
    assert_eq!(baseline, s.synthesise(&base).await.unwrap());

    // Shape, not just content: one passage and several read differently, and
    // "answered from nothing" is unmistakable.
    let mut two = base.clone();
    two.context.push(ContextItem {
        source: "Page B".into(),
        text: "the second passage".into(),
        note: hop_note(1),
    });
    assert!(baseline.contains("one passage"));
    assert!(s.synthesise(&two).await.unwrap().contains("2 passages"));

    let mut none = base.clone();
    none.context.clear();
    let empty_out = s.synthesise(&none).await.unwrap();
    assert!(
        empty_out.contains("Nothing on file"),
        "a no-evidence answer must not be mistakable for a sourced one: {empty_out}"
    );
}

/// `verify_answer` rejects whitespace, and names the model that produced it.
#[test]
fn verify_answer_rejects_an_empty_answer() {
    assert!(verify_answer("an answer", "m").is_ok());
    let err = verify_answer("  \n\t ", "some-model").unwrap_err();
    assert!(matches!(err, AnswerError::Invalid(ref m) if m.contains("some-model")), "{err}");
}

/// `hop_note` distinguishes a seed from each hop depth — the string a citation
/// and a prompt both lean on.
#[test]
fn hop_note_distinguishes_seed_from_each_depth() {
    assert_ne!(hop_note(0), hop_note(1));
    assert_ne!(hop_note(1), hop_note(2));
    assert!(hop_note(2).contains('2'));
}

/// `Inclusion` is the user-facing reading of `GraphHit::hops`, and it round-trips
/// every value the traversal can produce.
#[test]
fn inclusion_reads_hops_as_seed_or_graph() {
    assert_eq!(Inclusion::from_hops(0), Inclusion::Seed);
    assert_eq!(Inclusion::from_hops(1), Inclusion::Graph { hops: 1 });
    assert_eq!(Inclusion::from_hops(2), Inclusion::Graph { hops: 2 });
}

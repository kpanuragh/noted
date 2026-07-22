//! Local (entity-anchored) graph search, end to end — M2c Task 2.
//!
//! `noted_db::graph_search` is pure retrieval and knows nothing about
//! synthesis; `answer::AnswerProvider` is pure synthesis and knows nothing about
//! the database. This module is the seam, and it lives here for the same reason
//! `graph_write` does: `noted-db` must not depend back on `noted-index`.
//!
//! ```text
//! question
//!   -> search::hybrid                 [M1c: FTS + pgvector, RRF-fused]
//!   -> seed CHUNKS (hybrid returns PAGES; `seeds_from_pages` bridges)
//!   -> graph_search::local_search_chunks   [M2c Task 1: 1..2 hop traversal]
//!   -> graph_search::seed_entities         [the "why these results" surface]
//!   -> AnswerProvider::synthesise
//!   -> LocalAnswer { answer, citations, seed_entities }
//! ```
//!
//! # Citations are the product, not debug output
//!
//! Spec §3 makes "show your work" the differentiator. Every retrieved chunk
//! comes back as a `Citation` carrying its page, its content hash, and —
//! critically — **why it was retrieved**: hybrid search found it, or the graph
//! reached it N hops from something the question was about. A user who cannot
//! see why the system believes its own answer has no way to disbelieve it. That
//! is what `GraphHit::hops` was exposed for; `Inclusion` is its user-facing
//! reading.
//!
//! Citations are built from the RETRIEVAL ROWS, never from the model's output.
//! A provider is handed prose (`answer::ContextItem` carries no ids at all), so
//! no answerer — stub, real, or hallucinating — can invent, drop, or reattribute
//! a citation.

use crate::answer::{AnswerError, AnswerProvider, AnswerRequest, ContextItem, hop_note};
use noted_db::graph_search::{GraphHit, MAX_LOCAL_LIMIT, SeedChunk};
use noted_db::search;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum LocalSearchError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Answer(#[from] AnswerError),
}

/// Why a chunk is in the answer's evidence.
///
/// An enum rather than the raw `hops` integer because this crosses to the UI:
/// "found directly" and "reached through the graph" are different claims about
/// how much the system is asking the user to trust it, and a bare `0` on a
/// screen is not that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Inclusion {
    /// Hybrid search matched this chunk itself.
    Seed,
    /// The graph reached it: it evidences an edge `hops` steps from an entity a
    /// seed chunk anchored.
    Graph { hops: i32 },
}

impl Inclusion {
    /// `GraphHit::hops` is 0 for a seed and 1..=`MAX_HOPS` for a traversed hit —
    /// the traversal's `hops < $4` bound is what guarantees the upper end, and
    /// nothing can produce a negative. So the mapping is total on hop 0 and
    /// "anything else came from the walk".
    pub fn from_hops(hops: i32) -> Self {
        if hops == 0 {
            Inclusion::Seed
        } else {
            Inclusion::Graph { hops }
        }
    }

    /// The inverse, so the prompt's "why" line and the citation's `why` are
    /// derived from ONE value rather than from two independent reads of
    /// `GraphHit::hops` that could drift apart.
    fn hops(self) -> i32 {
        match self {
            Inclusion::Seed => 0,
            Inclusion::Graph { hops } => hops,
        }
    }
}

/// One piece of evidence, with everything needed to link back to its source.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Citation {
    pub page_id: Uuid,
    pub title: String,
    /// The chunk's content hash. GLOBAL, not per-workspace — chunks are
    /// content-addressed, so two workspaces holding identical text share one.
    /// It identifies the TEXT that was cited, and `page_id` is what scopes it.
    pub content_hash: String,
    pub snippet: String,
    pub why: Inclusion,
}

/// A seed entity — what the question turned out to be ABOUT.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SeedEntity {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LocalAnswer {
    pub answer: String,
    /// Ranked, best first — the same order the synthesiser saw its context in.
    pub citations: Vec<Citation>,
    pub seed_entities: Vec<SeedEntity>,
}

/// Answer a question from this workspace's notes and the graph over them.
///
/// # Signature: the spec says `local_search(workspace_id, question, k)`
///
/// It cannot: `search::hybrid`'s vector arm needs the question EMBEDDED, and
/// embedding lives behind this crate's `embed` feature (fastembed drags in an
/// ONNX runtime). Taking `q_vec` + `model_id` as parameters is exactly what
/// `hybrid` itself does and what `noted-server`'s search route already passes,
/// so this module stays unconditional — a caller without an embedder is a
/// caller that cannot do hybrid search at all, and the dependency belongs at
/// that caller, not here.
///
/// # `k` is clamped, in the same place and for the same reason as everywhere else
///
/// `1..=MAX_LOCAL_LIMIT`. `local_search_chunks` clamps its own limit, but
/// hybrid's does NOT, so an unclamped `k` here would either seed the traversal
/// from an unbounded page set (`k` huge) or from none at all (`k = 0` makes
/// `hybrid`'s `LIMIT 0` return nothing, and a zero-seed search returns no
/// evidence rather than an error). Clamping in the repository layer rather than
/// at the handler is this codebase's standing choice — `pages::MAX_RECENT_LIMIT`,
/// `graph_search::MAX_LOCAL_LIMIT` — because a second entry point that forgot to
/// clamp reopens the hole.
///
/// # No evidence means NO PROVIDER CALL
///
/// If hybrid matches nothing, this returns a fixed statement and **does not
/// invoke the answerer**. Not an optimisation: a model handed a question and no
/// context is being invited to answer it from its weights, and a fluent answer
/// with an empty citation list is the single worst output this surface can
/// produce — it looks exactly like a well-sourced one. `no_evidence_means_no_provider_call`
/// asserts the call count with `==`, guarded by a preceding call that DID run.
/// `user_id` filters every retrieved passage to pages the caller may READ.
///
/// Threaded all the way down rather than applied to the answer: the seeds come
/// from `hybrid` (which filters), and the traversal reaches chunks through
/// `page_chunks`, so a denied page could otherwise arrive as a GRAPH HOP even
/// though search itself would never return it. That hop is the subtle leak this
/// parameter closes.
pub async fn local_search(
    pool: &PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
    question: &str,
    q_vec: &[f32],
    model_id: &str,
    provider: &dyn AnswerProvider,
    k: i64,
) -> Result<LocalAnswer, LocalSearchError> {
    let k = k.clamp(1, MAX_LOCAL_LIMIT);

    let pages = search::hybrid(pool, workspace_id, user_id, question, q_vec, model_id, k).await?;
    let seeds = seeds_from_pages(pool, &pages).await?;

    // Retrieve MORE candidates than we will return, then reserve room for the
    // graph — see `reserve_graph_slots`. Passing `k` here (the obvious thing,
    // and what this function did first) is a silent product bug: `hybrid`
    // returns up to `k` pages, every one of them becomes a seed, and a seed
    // always outranks a traversed hit under `HOP_DECAY`. So at `k = 6` the six
    // seeds fill all six slots and the graph contributes NOTHING — local search
    // degenerates into hybrid search at exactly the sizes a user asks for.
    let budget = (k * CANDIDATE_MULTIPLIER).clamp(1, MAX_LOCAL_LIMIT);
    let hits = noted_db::graph_search::local_search_chunks(pool, workspace_id, user_id, &seeds, budget)
        .await?;
    let hits = reserve_graph_slots(hits, k);
    let entities = noted_db::graph_search::seed_entities(pool, workspace_id, &seeds).await?;
    let seed_entities: Vec<SeedEntity> = entities
        .into_iter()
        .map(|(id, name)| SeedEntity { id, name })
        .collect();

    let citations: Vec<Citation> = hits
        .iter()
        .map(|h: &GraphHit| Citation {
            page_id: h.page_id,
            title: h.title.clone(),
            content_hash: h.content_hash.clone(),
            snippet: h.snippet.clone(),
            why: Inclusion::from_hops(h.hops),
        })
        .collect();

    if citations.is_empty() {
        return Ok(LocalAnswer {
            answer: format!("No notes in this workspace bear on \"{}\".", question.trim()),
            citations,
            seed_entities,
        });
    }

    let req = AnswerRequest {
        question: question.to_string(),
        subjects: seed_entities.iter().map(|e| e.name.clone()).collect(),
        context: citations
            .iter()
            .map(|c| ContextItem {
                source: c.title.clone(),
                text: c.snippet.clone(),
                note: hop_note(c.why.hops()),
            })
            .collect(),
    };

    let answer = provider.synthesise(&req).await?;
    crate::answer::verify_answer(&answer, provider.model_id())?;

    Ok(LocalAnswer {
        answer,
        citations,
        seed_entities,
    })
}

/// How many candidates to retrieve per requested result before trimming to `k`.
///
/// The traversal must be allowed to surface hits that rank below the seeds, or
/// `reserve_graph_slots` has nothing to promote. Three is enough to reach past a
/// full page of seeds without making the traversal (which is NOT index-backed —
/// see the M2c design's risk table) meaningfully more expensive.
const CANDIDATE_MULTIPLIER: i64 = 3;

/// Keep `k` hits, guaranteeing the graph a share of them when it has any.
///
/// # Why this exists at all
///
/// Scores are comparable across seeds and traversed hits by construction
/// (`1/(RRF_K + seed_rank) · HOP_DECAY^hops · bottleneck`), so the obvious
/// "take the top `k`" is defensible — and it makes the graph invisible. The
/// seed-rank term spans a factor of ~2.6 across ranks 1..100 while one hop costs
/// a factor of 2, so within a small `k` EVERY seed outranks EVERY traversed hit.
/// Hybrid returns `k` pages whether or not they are any good (RRF has no
/// absolute threshold), so the tail of that list is noise that nonetheless
/// crowds out a page one edge from the question's subject.
///
/// That is not a tuning problem to be fixed by weakening `HOP_DECAY` — the decay
/// is right, and its monotonicity is separately pinned by
/// `ranking_is_monotone_in_hops_seed_rank_and_edge_weight`. It is a budgeting
/// problem: the product's claim is that the graph contributes, so the graph gets
/// slots.
///
/// # The reserve
///
/// Up to a third of `k` (at least one) is held for traversed hits, and only as
/// many as actually exist — a workspace with no graph loses nothing, and
/// `an_empty_graph_degrades_to_the_seed_chunks`' equivalent here still returns
/// pure hybrid results. Both sides keep their own ranking, and the merged list
/// is re-sorted by score so the output stays honestly ordered.
fn reserve_graph_slots(hits: Vec<GraphHit>, k: i64) -> Vec<GraphHit> {
    let k = k.max(0) as usize;
    if hits.len() <= k {
        return hits;
    }

    let (seeds, graph): (Vec<GraphHit>, Vec<GraphHit>) =
        hits.into_iter().partition(|h| h.hops == 0);

    let reserve = graph.len().min((k / 3).max(1));
    let take_seed = k.saturating_sub(reserve).min(seeds.len());
    let take_graph = (k - take_seed).min(graph.len());

    let mut out: Vec<GraphHit> = seeds
        .into_iter()
        .take(take_seed)
        .chain(graph.into_iter().take(take_graph))
        .collect();
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// Bridge `hybrid`'s PAGE hits to `local_search_chunks`' CHUNK seeds.
///
/// Two things are load-bearing here.
///
/// **Rank carries hybrid's ordering into the graph's ranking.** A seed's rank is
/// its page's 1-based position in the fused result, which is the same quantity
/// `graph_search`'s `1/(RRF_K + seed_rank)` term expects — M1c fuses on RANK
/// precisely because `ts_rank_cd` and cosine distance are incompatible scales,
/// and handing the raw `score` across instead would undo that. Every chunk of a
/// page inherits that page's rank: hybrid ranked the PAGE, and it picked only
/// one snippet, so there is no per-chunk signal to be had. A chunk shared by two
/// result pages therefore arrives twice at two ranks, which `local_search_chunks`
/// expects and collapses with `MIN`.
///
/// **One round trip, not one per page.** `= ANY($1)` over the whole id list.
///
/// TENANCY, since this query names both a page and a chunk and carries no
/// `workspace_id` of its own: its guard is upstream and total — `hybrid` only
/// ever returns pages with `p.workspace_id = $1 AND p.archived_at IS NULL`, so
/// the id list is already scoped, and `local_search_chunks` re-establishes the
/// workspace predicate on the way OUT anyway (content hashes are global, so it
/// must). The set of pages this can reach is exactly the set hybrid returned.
async fn seeds_from_pages(
    pool: &PgPool,
    pages: &[search::SearchHit],
) -> Result<Vec<SeedChunk>, sqlx::Error> {
    if pages.is_empty() {
        return Ok(Vec::new());
    }

    let ranks: HashMap<Uuid, i32> = pages
        .iter()
        .enumerate()
        .map(|(i, h)| (h.page_id, i as i32 + 1))
        .collect();
    let ids: Vec<Uuid> = pages.iter().map(|h| h.page_id).collect();

    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT page_id, content_hash
         FROM page_chunks
         WHERE page_id = ANY($1)
         ORDER BY page_id, chunk_index",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(page_id, content_hash)| {
            ranks.get(&page_id).map(|rank| SeedChunk {
                content_hash,
                rank: *rank,
            })
        })
        .collect())
}

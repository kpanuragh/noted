//! The community worker: the hot path, the cold path, and the churn threshold
//! that decides between them.
//!
//! # The shape, and why it is this shape
//!
//! Incremental community detection is a near-open problem — re-clustering a
//! whole graph on every edit is impossible and no published system does the
//! incremental version well. So this does not pretend to (design §2.1):
//!
//!   * **Hot path** ([`CommunityWorker::hot_reassign`]) — when edges change,
//!     each affected entity is moved into the community of its most
//!     strongly-connected neighbour. O(degree), one query per entity, inline,
//!     no clustering. The partition stays *available* and approximately right.
//!   * **Cold path** ([`CommunityWorker::cold_run`]) — full Louvain over the
//!     workspace's clusterable graph, swapped in atomically. The partition
//!     becomes *exact*.
//!   * **Churn** decides when. `graph_churn.edges_changed` accumulates, and
//!     crossing [`CHURN_THRESHOLD_FRACTION`] of the workspace's clusterable
//!     edge count triggers a cold run, which resets the counter and stamps
//!     `last_full_run_at`.
//!
//! Cheap continuously, correct eventually, never blocking — the same shape as
//! M1a's projection debounce and M1b's set-difference queue.
//!
//! # Where the graph comes from
//!
//! Both paths read [`noted_db::community::clusterable_graph`] /
//! [`noted_db::community::strongest_clustered_neighbour`], which are built from
//! ONE shared SQL definition of which edges clustering may see (entities with
//! at least one edge sourced from a live, non-archived page of THIS workspace).
//! Read that constant's doc comment before changing anything here: if the two
//! paths ever disagree about liveness, the convergence property below compares
//! two different graphs and fails in a way that looks exactly like clusterer
//! non-determinism.
//!
//! # No worker loop
//!
//! Unlike `extract_worker`/`worker`, there is no `drain` and no batch polling.
//! There is no queue to drain: clustering is a single whole-workspace
//! operation, and the thing that schedules it is the churn counter, not a set
//! difference over pending rows. [`CommunityWorker::on_edges_changed`] is the
//! production entry point a writer calls after changing a workspace's edges.
use noted_db::community;
use uuid::Uuid;

use crate::louvain::louvain;

/// Fraction of a workspace's clusterable edges that must change before the
/// cold path runs. Design §2.1's "~5%".
pub const CHURN_THRESHOLD_FRACTION: f64 = 0.05;

#[derive(Debug, thiserror::Error)]
pub enum CommunityError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// What [`CommunityWorker::on_edges_changed`] did, so a caller (and a test) can
/// tell the approximate path from the exact one without inspecting the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeChangeOutcome {
    /// Entities the hot path actually moved (those with a clustered neighbour).
    pub reassigned: usize,
    /// Whether the churn threshold tripped and a full re-cluster ran.
    pub cold_run: bool,
}

/// Workspace-scoped, always. Clustering is a per-tenant operation over a
/// per-tenant graph, and there is no unscoped variant of it the way
/// `ExtractWorker::new` is the unscoped variant of `new_scoped` — a cross-tenant
/// cold run is not a faster whole-instance job, it is a data-integrity bug.
pub struct CommunityWorker {
    pool: sqlx::PgPool,
    workspace_id: Uuid,
}

impl CommunityWorker {
    pub fn new(pool: sqlx::PgPool, workspace_id: Uuid) -> Self {
        Self { pool, workspace_id }
    }

    /// THE COLD PATH. Full Louvain over the workspace's clusterable graph,
    /// swapped in atomically, churn reset.
    ///
    /// Returns the number of communities in the new partition.
    ///
    /// Steps, in the only order that is correct:
    ///
    /// 1. Read the clusterable graph. Nodes arrive **ordered by
    ///    `entities.name`**, and their POSITION in that vector is the node index
    ///    handed to the clusterer. That is the canonicalisation the whole
    ///    determinism story rests on — see `noted_db::community::clusterable_graph`
    ///    for why the key is `name` and never `id`.
    /// 2. Cluster ([`louvain`]), which takes an edge list precisely so that
    ///    index assignment cannot leak in from a graph object.
    /// 3. Map community members back from indices to entity ids and
    ///    `swap_partition` — one transaction, all-or-nothing, preserving the
    ///    rows (and summaries) of communities whose membership did not change.
    /// 4. Only THEN reset churn and stamp `last_full_run_at`. After, never
    ///    before: a swap that fails must leave the workspace still owing a cold
    ///    run, not looking freshly clustered.
    ///
    /// Every community is written at level 0. `louvain` returns the flattened
    /// final level rather than the hierarchy, so there is nothing else to write
    /// yet; `level` exists in the schema because the hierarchy is the natural
    /// extension, and a level-aware cold run would fill it in without a
    /// migration.
    pub async fn cold_run(&self) -> Result<usize, CommunityError> {
        let (nodes, edges) = community::clusterable_graph(&self.pool, self.workspace_id).await?;

        let index: std::collections::HashMap<Uuid, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (*id, i))
            .collect();

        // `filter_map` rather than indexing, and it is not defensive noise.
        // `clusterable_graph` is TWO round trips, so an extraction committing
        // between them can produce an edge whose endpoints were not in the node
        // list — a real, reachable race on a live instance. Indexing would panic
        // the worker; dropping the edge clusters the graph as it stood at the
        // first query, which is a consistent snapshot and is corrected by the
        // next cold run. (Widening this to one transaction would also be
        // correct; it is not worth a longer-held read for an edge that is about
        // to be re-read anyway.)
        let indexed: Vec<(usize, usize, f64)> = edges
            .iter()
            .filter_map(|(a, b, w)| Some((*index.get(a)?, *index.get(b)?, *w)))
            .collect();

        let partition = louvain(nodes.len(), &indexed);

        let rows: Vec<(i32, Vec<Uuid>)> = partition
            .communities()
            .iter()
            .map(|members| (0, members.iter().map(|&i| nodes[i].0).collect()))
            .collect();

        community::swap_partition(&self.pool, self.workspace_id, &rows).await?;
        community::mark_full_run(&self.pool, self.workspace_id).await?;

        Ok(rows.len())
    }

    /// THE HOT PATH. Each entity in `entities` joins the community of its most
    /// strongly-connected clusterable neighbour.
    ///
    /// Returns how many were actually moved. An entity with no clustered
    /// neighbour is LEFT ALONE — not orphaned into a fresh singleton community
    /// and not removed from the community it is in. There is no evidence to act
    /// on in that case, and the two alternatives are both worse: inventing a
    /// singleton manufactures a community that the next cold run will delete
    /// (and that M2b-3 would try to summarise in the meantime), and removing the
    /// entity makes the partition less complete than it was, which is the one
    /// thing a path whose job is availability must never do.
    ///
    /// This is an approximation and is not claimed to be anything else. It is
    /// bounded by the churn threshold and corrected by the next cold run; the
    /// convergence property in `tests/communities.rs` is what licenses it.
    ///
    /// # Pass the entities an edit TOUCHED, not the whole graph
    ///
    /// Quality degrades sharply with the size of `entities`, and the mechanism
    /// is worth understanding rather than discovering. The moves are applied in
    /// sequence and each one reads the state the previous ones left, so feeding
    /// in an entire workspace makes every entity chase its strongest
    /// neighbour's *current* community and the moves cascade — measured on two
    /// well-separated 4-cliques, which collapse into a single community. That
    /// is not a bug in the reassignment rule; it is what "assign to your
    /// strongest neighbour's community" means when applied transitively, and it
    /// is exactly why this is the cheap path and not the correct one. The
    /// production caller passes the handful of entities an edit actually
    /// touched, where the cascade cannot get started. A caller with a large
    /// affected set has, by construction, also generated large churn, so the
    /// cold path is about to fire anyway.
    ///
    /// Not one transaction across all entities, deliberately. Each reassignment
    /// is individually atomic (`community::reassign_entity`); a failure partway
    /// leaves the earlier ones applied, which is a perfectly good partition —
    /// every partial state of a hot-path run is itself just a different
    /// approximation, and the churn counter still owes a cold run either way.
    pub async fn hot_reassign(&self, entities: &[Uuid]) -> Result<usize, CommunityError> {
        let mut moved = 0usize;
        for &entity_id in entities {
            let target =
                community::strongest_clustered_neighbour(&self.pool, self.workspace_id, entity_id)
                    .await?;
            if let Some(community_id) = target {
                community::reassign_entity(&self.pool, self.workspace_id, entity_id, community_id)
                    .await?;
                moved += 1;
            }
        }
        Ok(moved)
    }

    /// Run the cold path IF churn has crossed the threshold. Returns whether it
    /// ran.
    ///
    /// The threshold is `ceil(FRACTION * clusterable_edges)`, and the CEILING is
    /// what makes small workspaces work: 5% of 4 edges is 0.2, which truncates
    /// to a threshold of zero, and a workspace whose threshold is zero is one
    /// that never uses the hot path at all. Rounding up means a small workspace
    /// cold-runs on its first changed edge — cheap, because it is small — and a
    /// large one waits for a real 5%.
    ///
    /// An explicit `max(1)` floor on top of that was written here first and then
    /// removed, because it is unreachable: `ceil` already returns at least 1 for
    /// any non-empty graph, and on an EMPTY graph (threshold 0) the guard below
    /// has already returned for `changed <= 0`, so both variants cold-run. It
    /// would have shipped as a mechanism the doc comment claimed and no mutation
    /// could kill — the exact trap Task 2 found in the Louvain determinism
    /// mechanisms.
    pub async fn cold_run_if_due(&self) -> Result<bool, CommunityError> {
        // A workspace that has changed nothing is never due, whatever the
        // threshold works out to. This is also what keeps an idle workspace from
        // re-clustering on every call.
        let (changed, _) = community::churn(&self.pool, self.workspace_id).await?;
        if changed <= 0 {
            return Ok(false);
        }

        let edges = community::clusterable_edge_count(&self.pool, self.workspace_id).await?;
        let threshold = ((edges as f64) * CHURN_THRESHOLD_FRACTION).ceil() as i64;

        if changed < threshold {
            return Ok(false);
        }

        self.cold_run().await?;
        Ok(true)
    }

    /// THE PRODUCTION ENTRY POINT. Call this after a write has changed a
    /// workspace's edges.
    ///
    /// Hot-reassign the affected entities, add the change to the churn counter,
    /// then cold-run if that pushed churn over the threshold. In that order:
    /// the hot path keeps the partition usable immediately, and the cold path —
    /// when it fires — overwrites whatever the hot path just did with the exact
    /// answer, so doing it second wastes nothing and doing it first would leave
    /// the run's own changes unabsorbed.
    ///
    /// `affected` is supplied by the caller rather than derived here, because
    /// the entities touched by an edit are known to the writer and are NOT
    /// recoverable afterwards: an edit that removes a chunk from a page makes
    /// its edges dead, so by the time this is called they are already invisible
    /// to every clusterable query — exactly the entities most in need of
    /// reassignment.
    pub async fn on_edges_changed(
        &self,
        affected: &[Uuid],
        edges_changed: i64,
    ) -> Result<EdgeChangeOutcome, CommunityError> {
        let reassigned = self.hot_reassign(affected).await?;
        community::bump_churn(&self.pool, self.workspace_id, edges_changed).await?;
        let cold_run = self.cold_run_if_due().await?;
        Ok(EdgeChangeOutcome {
            reassigned,
            cold_run,
        })
    }
}

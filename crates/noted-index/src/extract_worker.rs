//! The extraction worker: poll `graph::pending_extraction`, run each pending
//! chunk through an `ExtractionProvider`, and write the result into every
//! workspace's graph that references the chunk.
//!
//! Mirrors `worker.rs` (M1b's embedding worker) in shape and in the lessons
//! it encodes:
//!   1. The queue is a QUERY (`graph::pending_extraction`), not a table — no
//!      status column, no claim/lease state. Crash-safety comes from the set
//!      difference re-evaluating on every poll.
//!   2. Per-chunk failure isolation: one un-extractable chunk must not stall
//!      the whole drain.
//!   3. Resumability: killing the process mid-drain and restarting picks up
//!      exactly the remaining work, never redoing what already landed.
//!
//! One thing does NOT mirror `worker.rs`: embeddings batch N texts into ONE
//! provider call, so a batch failure needs a second, chunk-by-chunk retry
//! pass to isolate the poison chunk. Extraction calls `extract()` once PER
//! CHUNK to begin with (there is no batched-call shape to unwind), so that
//! isolation falls out of the natural per-item loop below with no extra
//! retry pass needed.
use std::sync::Arc;

use uuid::Uuid;

use crate::extract::{Extraction, ExtractionProvider};
use crate::community_worker::CommunityWorker;
use crate::graph_write::apply_extraction;

/// How many pending chunks to poll per round trip. Extraction is one
/// provider call per chunk (unlike embedding's batched calls), so this bounds
/// how much work one `run_once` attempts, not how many texts go in one
/// network/inference call.
pub const BATCH_SIZE: i64 = 20;

/// How many consecutive batches `drain` tolerates making zero progress on
/// before giving up. Mirrors `worker::MAX_CONSECUTIVE_FAILURES`: the queue is
/// a set difference with no state, so a chunk `extract()` can never handle is
/// returned by `pending_extraction` forever. Without a cap, a queue that has
/// become nothing-but-poison (or a provider that can't connect at all) would
/// spin indefinitely instead of terminating with a clear error.
pub const MAX_CONSECUTIVE_FAILURES: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum ExtractWorkerError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    /// A community-maintenance failure after the edges were already committed.
    ///
    /// PROPAGATED, not swallowed, for the same reason a `sqlx::Error` in step
    /// 2-3 is: `CommunityError` is a database failure in all its variants, and
    /// grinding on past a dead database silently loses work rather than
    /// isolating a bad chunk. The extraction itself is NOT lost — its edges and
    /// its `chunk_extractions` marker committed in their own transaction before
    /// this ran — so the cost of the failure is bounded to stale communities
    /// for that workspace, which the next cold run corrects.
    #[error("community maintenance failed after edges were written: {0}")]
    Community(#[from] crate::community_worker::CommunityError),
    #[error(
        "extraction stalled: {batches} consecutive batch(es) made no progress (the last had \
         {chunks} chunk(s) pending); {extracted} chunk(s) were extracted before giving up. \
         Either the provider is unreachable/broken, or those chunks cannot be extracted. \
         Re-running is safe and will retry them."
    )]
    Stalled {
        batches: usize,
        chunks: usize,
        extracted: usize,
    },
}

pub struct ExtractWorker {
    pool: sqlx::PgPool,
    provider: Arc<dyn ExtractionProvider>,
    /// Scopes `pending_extraction`'s POLL — and only the poll. `None` (the
    /// default via `new`) drains the whole instance, which is what the CLI
    /// wants. `Some(id)` (via `new_scoped`) restricts the polled queue to
    /// chunks referenced by a live page in that one workspace, so a per-tenant
    /// run does not pull every other workspace's pending chunks into its
    /// batches. See `noted_db::graph::pending_extraction`.
    ///
    /// IT DOES NOT SCOPE THE WRITE. Once a chunk is polled, `process_batch`
    /// calls `graph::workspaces_for_chunk` UNSCOPED and applies the extraction
    /// to EVERY workspace with a live page referencing that chunk — including
    /// workspaces this worker is not scoped to — marking each one extracted.
    /// A scoped run therefore does bounded work on other tenants' graphs
    /// whenever a chunk is shared (content-addressed chunks are shared exactly
    /// when two workspaces hold byte-identical text — M1b).
    ///
    /// With a DETERMINISTIC extractor that is harmless and arguably a
    /// courtesy: the other workspace gets an identical graph slightly early.
    /// Under a real LLM it is not, because extraction is non-deterministic — a
    /// per-tenant run would REWRITE another tenant's already-correct edges for
    /// that chunk with a different result. Behaviour is unchanged for now; the
    /// decision is recorded in `.superpowers/sdd/progress.md`.
    workspace_id: Option<Uuid>,
}

impl ExtractWorker {
    /// Whole-instance worker — polls `pending_extraction` unscoped. This is
    /// what the CLI uses.
    pub fn new(pool: sqlx::PgPool, provider: Arc<dyn ExtractionProvider>) -> Self {
        Self {
            pool,
            provider,
            workspace_id: None,
        }
    }

    /// Workspace-scoped worker — polls only chunks referenced by a live page
    /// in `workspace_id`. Use for per-tenant extraction runs and for tests on
    /// a shared dev database, where an unscoped poll would also return every
    /// other workspace's pending chunks.
    pub fn new_scoped(
        pool: sqlx::PgPool,
        provider: Arc<dyn ExtractionProvider>,
        workspace_id: Uuid,
    ) -> Self {
        Self {
            pool,
            provider,
            workspace_id: Some(workspace_id),
        }
    }

    /// Process one polled batch of pending chunks.
    ///
    /// For each `(content_hash, text)`:
    ///   1. Extract the text ONCE (`provider.extract`). A provider error here
    ///      is a poison chunk — logged and skipped, NOT propagated, so it
    ///      cannot stall the chunks around it. It stays in the queue (there
    ///      is nowhere else for it to go).
    ///   2. Resolve every workspace whose live page currently references
    ///      this chunk (`graph::workspaces_for_chunk`) — a chunk is
    ///      content-addressed and can be shared across workspaces (M1b), and
    ///      each one needs its own copy of the extraction written. NOTE this
    ///      lookup is UNSCOPED even when the worker is `new_scoped` — see
    ///      `workspace_id`'s docs for what that means and why it is left as
    ///      is.
    ///   3. Apply the SAME extraction to EACH workspace's graph
    ///      (`apply_extraction`, which resolves entities/edges and calls
    ///      `replace_chunk_edges` — scoped to that one workspace, and which
    ///      writes that workspace's `chunk_extractions` marker in the SAME
    ///      transaction as its edges).
    ///
    /// There is deliberately no separate "mark extracted" step. The marker is
    /// per-workspace (migration `0008_chunk_extractions_workspace.sql`), so it
    /// belongs inside each workspace's edge transaction: a crash partway
    /// through the loop leaves the workspaces already written marked and the
    /// rest legitimately pending, and the next poll picks up exactly the
    /// remainder. Nothing is stranded and nothing is double-written.
    ///
    /// A failure in step 2-3 (a `sqlx::Error`) is a DATABASE failure, not a
    /// poison chunk — like `worker.rs`'s storage writes, it propagates
    /// immediately rather than being swallowed, because retrying past a dead
    /// database would just silently lose work rather than isolate a bad
    /// chunk.
    async fn process_batch(&self, batch: &[(String, String)]) -> Result<usize, ExtractWorkerError> {
        let model_id = self.provider.model_id().to_string();
        let mut succeeded = 0usize;

        for (content_hash, text) in batch {
            // NOTE for real providers (Ollama/HTTP): `extract` is async and
            // must do its I/O (or local inference) off the tokio worker
            // thread — an HTTP call should use an async client (reqwest,
            // used by `extract_providers::OllamaExtractor`), and CPU-bound
            // local inference should go through `tokio::task::spawn_blocking`
            // inside the provider. `StubExtractor` is pure/instant so this
            // doesn't matter here, but a blocking provider implementation
            // would stall every other task sharing this runtime.
            let extraction: Extraction = match self.provider.extract(text).await {
                Ok(e) => e,
                Err(err) => {
                    tracing::warn!(
                        hash = %content_hash,
                        error = %err,
                        "chunk could not be extracted; skipping it and leaving it in the queue"
                    );
                    continue;
                }
            };

            let workspaces: Vec<Uuid> =
                noted_db::graph::workspaces_for_chunk(&self.pool, content_hash).await?;
            if workspaces.is_empty() {
                // Raced with an edit that orphaned the chunk between the poll
                // and now. Nothing to attribute the extraction to; leave it
                // — if it is truly orphaned, `pending_extraction` (which
                // joins through live `page_chunks`) will simply stop
                // returning it.
                tracing::warn!(
                    hash = %content_hash,
                    "no live workspace references this chunk any more; skipping"
                );
                continue;
            }

            for workspace_id in &workspaces {
                let applied = apply_extraction(
                    &self.pool,
                    *workspace_id,
                    content_hash,
                    &model_id,
                    &extraction,
                )
                .await?;

                // THE WIRING. Until this call existed, `CommunityWorker` had no
                // production caller at all: it was fully built, fully tested,
                // and reachable only from a test. Writing a page updated the
                // graph's edges and left its communities frozen forever.
                //
                // HERE, per workspace, inside the fan-out loop — not after the
                // batch and not after the drain — for three reasons:
                //   * Communities are per-workspace, and so is churn and the
                //     cold-run threshold. A batch-level call would have to
                //     re-derive which workspaces changed and by how much, which
                //     is precisely what this loop already knows.
                //   * `apply_extraction` has just told us the exact entity set
                //     this chunk touched. `hot_reassign` cascades transitively,
                //     so a broader set — anything reconstructed later — would
                //     merge communities that should stay distinct.
                //   * A shared chunk fans out to N workspaces with N different
                //     graphs; each needs its own decision about whether the cold
                //     path is due.
                //
                // Deliberately NOT inside `replace_chunk_edges`' transaction:
                // clustering is derived state, and a Louvain run that failed
                // would otherwise roll back an extraction that succeeded. The
                // error DOES propagate (see `ExtractWorkerError::Community`) —
                // but the edges and the extracted-marker have already committed,
                // so what is lost is a churn bump, not work.
                if !applied.entities.is_empty() {
                    CommunityWorker::new(self.pool.clone(), *workspace_id)
                        .on_edges_changed(&applied.entities, applied.edges)
                        .await?;
                }
            }

            succeeded += 1;
        }

        Ok(succeeded)
    }

    /// Poll and process ONE batch. Returns the number of chunks successfully
    /// extracted and marked (which may be less than the batch size, or even
    /// zero, if some/all of the batch's chunks were poison — that is not an
    /// error by itself; see `drain` for what actually terminates the loop).
    pub async fn run_once(&self) -> Result<usize, ExtractWorkerError> {
        let model_id = self.provider.model_id().to_string();
        let batch = noted_db::graph::pending_extraction(
            &self.pool,
            &model_id,
            self.workspace_id,
            BATCH_SIZE,
        )
        .await?;
        self.process_batch(&batch).await
    }

    /// Run batches until the queue is empty. Safe to kill at any moment: the
    /// queue is a set difference with no in-progress state, so the next run
    /// picks up exactly what is left.
    ///
    /// Terminates on one of three things: an empty queue (the poll itself
    /// returns nothing), a database error, or `MAX_CONSECUTIVE_FAILURES`
    /// batches in a row that extracted nothing (every chunk in them was
    /// poison, or the provider cannot connect at all).
    pub async fn drain(&self) -> Result<usize, ExtractWorkerError> {
        let model_id = self.provider.model_id().to_string();
        let mut total = 0usize;
        let mut consecutive_failures = 0usize;

        loop {
            let batch = noted_db::graph::pending_extraction(
                &self.pool,
                &model_id,
                self.workspace_id,
                BATCH_SIZE,
            )
            .await?;
            if batch.is_empty() {
                return Ok(total);
            }

            let n = self.process_batch(&batch).await?;
            if n == 0 {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    return Err(ExtractWorkerError::Stalled {
                        batches: consecutive_failures,
                        chunks: batch.len(),
                        extracted: total,
                    });
                }
            } else {
                total += n;
                // Progress was made, so whatever failed before was not fatal.
                consecutive_failures = 0;
            }
        }
    }
}

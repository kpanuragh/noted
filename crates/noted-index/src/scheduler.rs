//! The background indexer: makes writing a page enough to make it searchable.
//!
//! # Why this exists
//!
//! Every stage of the pipeline was already crash-safe and resumable, and every
//! one of them was driven by a human running a CLI. So a page written at 09:00
//! was invisible to search until somebody remembered to run `noted-index` —
//! which, in a product whose entire pitch is "ask your notes", is the difference
//! between working and not.
//!
//! # Staleness is VISIBLE, not hidden
//!
//! The scheduler reports what it is behind on rather than pretending to be
//! current. M1a's projection debounce set this precedent and it holds here: a
//! user who can see "12 passages still indexing" understands an incomplete
//! answer, while a user who cannot see it concludes the product is broken.
//! [`IndexingStatus`] is what the API surfaces.
//!
//! # It owns no queue
//!
//! Each pass re-evaluates the same set-difference queries the CLI uses. There is
//! no in-memory backlog, no claim, no lease — kill the process mid-pass and the
//! next tick picks up exactly the remainder, because the queue is a QUERY. That
//! property was expensive to establish and this module must not undo it.
use std::sync::Arc;
use std::time::Duration;

use noted_db::PgPool;
use tokio::task::JoinHandle;

use crate::extract::ExtractionProvider;
use crate::extract_worker::ExtractWorker;
use crate::provider::EmbeddingProvider;
use crate::worker::Worker;

/// How long to wait between passes when the last one found nothing to do.
///
/// Long enough that an idle instance is not a busy-loop against Postgres, short
/// enough that a page written now is searchable within about the time it takes
/// to switch tabs and come back.
pub const IDLE_INTERVAL: Duration = Duration::from_secs(15);

/// How long to wait when the last pass DID find work.
///
/// Much shorter: a backlog should drain at the speed of the machine, not at the
/// speed of the idle poll. A fresh import of a thousand pages must not take four
/// hours because the scheduler slept fifteen seconds between batches.
pub const BUSY_INTERVAL: Duration = Duration::from_millis(250);

/// How many idle passes between graph sweeps.
///
/// The reaper collects rows nothing can reach, so it is never urgent and its
/// cost is two full-table deletes. Running it every pass would spend that on
/// nothing almost every time; running it only at startup would let residue
/// accumulate for the life of a long-running process. Every 40 idle passes is
/// roughly every ten minutes at `IDLE_INTERVAL`.
pub const SWEEP_EVERY_N_IDLE_PASSES: u32 = 40;

/// What the indexer is behind on, for the UI to show honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct IndexingStatus {
    /// Chunks embedded under the current model, over chunks that need it.
    pub embedded: i64,
    pub embed_total: i64,
    /// The same for graph extraction. Zero-of-zero when no extraction provider
    /// is configured, which is honest: nothing is pending because nothing can
    /// run.
    pub extracted: i64,
    pub extract_total: i64,
}

impl IndexingStatus {
    /// Is everything the pipeline can do, done?
    pub fn is_current(&self) -> bool {
        self.embedded >= self.embed_total && self.extracted >= self.extract_total
    }

    /// How many units of work remain. What a UI counts down.
    pub fn pending(&self) -> i64 {
        (self.embed_total - self.embedded).max(0) + (self.extract_total - self.extracted).max(0)
    }
}

/// Read the current backlog for one workspace, or the whole instance.
pub async fn status(
    pool: &PgPool,
    embed_model: &str,
    extract_model: Option<&str>,
    workspace_id: Option<uuid::Uuid>,
) -> Result<IndexingStatus, sqlx::Error> {
    let (embedded, embed_total) =
        noted_db::chunks::progress(pool, embed_model, workspace_id).await?;

    // No extraction provider configured means no extraction is pending, rather
    // than "everything is pending forever". Reporting a backlog that nothing
    // will ever drain would make `is_current` permanently false and the UI
    // permanently alarmed.
    let (extracted, extract_total) = match extract_model {
        Some(model) => noted_db::graph::extraction_progress(pool, model, workspace_id).await?,
        None => (0, 0),
    };

    Ok(IndexingStatus {
        embedded,
        embed_total,
        extracted,
        extract_total,
    })
}

/// A running background indexer.
///
/// Dropping this does NOT stop the loop — hold it and call [`Scheduler::stop`],
/// or let process exit take it. `stop` is what tests use, and what a graceful
/// shutdown should use so a pass finishes its current batch rather than being
/// torn out mid-write.
pub struct Scheduler {
    handle: JoinHandle<()>,
    /// The authoritative stop signal, checked at the top of every pass.
    ///
    /// A `Notify` ALONE is not enough, and the test caught it by hanging:
    /// `notify_waiters` only wakes tasks already parked at `notified()`, so a
    /// stop arriving while the loop is inside `run_pass` is dropped on the
    /// floor. The loop then sleeps a full interval, wakes to a signal nobody is
    /// sending a second time, and `stop()` awaits a task that never exits.
    ///
    /// A flag cannot be missed, because it is STATE rather than an EVENT.
    stopping: Arc<std::sync::atomic::AtomicBool>,
    cancel: Arc<tokio::sync::Notify>,
}

impl Scheduler {
    /// Start indexing in the background.
    ///
    /// `extractor` is optional and absent by default for the same reason the
    /// CLI gates it behind `NOTED_EXTRACT`: there is no LLM in most
    /// deployments, and a stub silently building a meaningless graph is worse
    /// than no graph.
    /// `summariser` is optional for the same reason `extractor` is: not every
    /// deployment has a model. Absent, community summaries are never written
    /// and global search reports its themes as unsummarised — which is exactly
    /// what it did for every deployment before this was wired, because
    /// `SummaryWorker` was reachable only from the CLI and from global
    /// search's lazy REFRESH path. Refresh regenerates a summary that has gone
    /// stale; it has never created a missing one. So a server that had
    /// clustered its notes into themes could sit there indefinitely with zero
    /// summaries and nothing to answer "across everything" from.
    pub fn start(
        pool: PgPool,
        embedder: Arc<dyn EmbeddingProvider>,
        extractor: Option<Arc<dyn ExtractionProvider>>,
        summariser: Option<Arc<dyn crate::summary::SummaryProvider>>,
    ) -> Result<Self, crate::worker::WorkerError> {
        let cancel = Arc::new(tokio::sync::Notify::new());
        let stop = cancel.clone();
        let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping_task = stopping.clone();

        let sweep_pool = pool.clone();
        let embed_worker = Worker::new(pool.clone(), embedder)?;
        let extract_worker = extractor.map(|e| ExtractWorker::new(pool.clone(), e));
        let summary_pool = pool.clone();

        let handle = tokio::spawn(async move {
            use std::sync::atomic::Ordering;
            let mut idle_passes: u32 = 0;
            loop {
                // Before the pass: a `stop()` that landed while the previous
                // sleep was running must not cost another whole pass.
                if stopping_task.load(Ordering::Relaxed) {
                    break;
                }

                let did_work = run_pass(
                    &embed_worker,
                    extract_worker.as_ref(),
                    summariser.as_ref(),
                    &summary_pool,
                )
                .await;

                // And after it: a `stop()` that landed DURING the pass must not
                // cost an interval of sleep. This is the check that was missing
                // when `stop()` hung.
                if stopping_task.load(Ordering::Relaxed) {
                    break;
                }

                let wait = if did_work {
                    idle_passes = 0;
                    BUSY_INTERVAL
                } else {
                    idle_passes = idle_passes.saturating_add(1);
                    // Swept only while IDLE, and only every so often: a sweep
                    // during a backlog would compete with indexing for the same
                    // database, to collect rows that are in nobody's way.
                    if idle_passes % SWEEP_EVERY_N_IDLE_PASSES == 0 {
                        match noted_db::graph::reap_graph(&sweep_pool, None).await {
                            Ok(r) if r.edges > 0 || r.entities > 0 => {
                                tracing::info!(
                                    edges = r.edges,
                                    entities = r.entities,
                                    "swept graph residue from archived pages"
                                );
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "graph sweep failed; will retry"),
                        }
                    }
                    IDLE_INTERVAL
                };

                // Wait for the interval OR a stop signal, whichever comes
                // first. `select!` rather than a sleep followed by a check, so
                // shutdown does not have to wait out a full idle interval.
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = stop.notified() => break,
                }
            }
            tracing::info!("indexing scheduler stopped");
        });

        Ok(Self {
            handle,
            stopping,
            cancel,
        })
    }

    /// Ask the loop to stop after its current pass, and wait for it.
    pub async fn stop(self) {
        // Order matters: set the flag FIRST, so a loop that is mid-pass sees it
        // at its next check whether or not it is parked to receive the notify.
        self.stopping
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // `notify_one`, not `notify_waiters`: it leaves a permit behind if the
        // loop is not currently parked, so the wake-up cannot be lost. The
        // notify only SHORTENS the wait — the flag is what guarantees the exit.
        self.cancel.notify_one();
        let _ = self.handle.await;
    }
}

/// One pass over both queues. Returns whether it did anything.
///
/// A failure in either stage is LOGGED AND SWALLOWED, deliberately. This is a
/// background loop: propagating would kill the scheduler for the lifetime of the
/// process, so a transient database blip would silently stop all indexing until
/// someone restarted the server. The CLI, which a human is watching, still
/// reports errors loudly — that is the right place for them.
/// How many workspaces one pass will summarise for.
///
/// Bounded so a single tenant with a large backlog cannot starve the others.
/// The rest stay pending: the queue is a query, so there is no cursor to lose
/// and the next pass simply sees them again.
const SUMMARY_WORKSPACES_PER_PASS: i64 = 4;

async fn run_pass(
    embed: &Worker,
    extract: Option<&ExtractWorker>,
    summariser: Option<&Arc<dyn crate::summary::SummaryProvider>>,
    pool: &sqlx::PgPool,
) -> bool {
    let mut did_work = false;

    match embed.run_once().await {
        Ok(crate::worker::BatchOutcome::Embedded(0)) => {}
        Ok(crate::worker::BatchOutcome::Embedded(n)) => {
            did_work = true;
            tracing::debug!(embedded = n, "indexed a batch");
        }
        // A FAILED batch is NOT progress, and must not set `did_work`.
        //
        // The failed chunks stay in the queue (there is nowhere else for them to
        // go), so the next pass polls the same batch. Counting that as work
        // would hold the loop at the 250ms busy interval and retry a
        // permanently-poisoned chunk four times a second forever — a hot loop
        // against both Postgres and the embedding provider, for work that will
        // never succeed. Backing off to the idle interval keeps retrying (a
        // failure may be transient) at a rate that costs nothing if it is not.
        Ok(crate::worker::BatchOutcome::Failed(n)) => {
            tracing::warn!(
                failed = n,
                "a batch could not be embedded; it stays queued and will be retried at the idle \
                 interval"
            );
        }
        Err(e) => tracing::warn!(error = %e, "embedding pass failed; will retry"),
    }

    if let Some(worker) = extract {
        match worker.run_once().await {
            Ok(n) => {
                if n > 0 {
                    did_work = true;
                    tracing::debug!(extracted = n, "extracted a batch");
                }
            }
            Err(e) => tracing::warn!(error = %e, "extraction pass failed; will retry"),
        }
    }

    if let Some(provider) = summariser {
        let model_id = provider.model_id().to_string();
        match crate::summary_worker::workspaces_with_pending_summaries(
            pool,
            &model_id,
            SUMMARY_WORKSPACES_PER_PASS,
        )
        .await
        {
            Ok(workspaces) => {
                for ws in workspaces {
                    let worker = crate::summary_worker::SummaryWorker::new(
                        pool.clone(),
                        Arc::clone(provider),
                        ws,
                    );
                    match worker.run_once().await {
                        Ok(pass) => {
                            // `marked_stale` is deliberately NOT progress: it
                            // makes no model call and leaves the community
                            // pending, so counting it would hold the loop at
                            // the busy interval re-marking the same rows
                            // forever. Same reasoning as a failed embed batch
                            // above.
                            if pass.regenerated > 0 {
                                did_work = true;
                                tracing::info!(
                                    workspace = %ws,
                                    regenerated = pass.regenerated,
                                    marked_stale = pass.marked_stale,
                                    failed = pass.failed,
                                    "summarised communities"
                                );
                            } else if pass.failed > 0 {
                                tracing::warn!(
                                    workspace = %ws,
                                    failed = pass.failed,
                                    "no community could be summarised; they stay queued"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(workspace = %ws, error = %e, "summary pass failed; will retry")
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not poll for pending summaries"),
        }
    }

    did_work
}

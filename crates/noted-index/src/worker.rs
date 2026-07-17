use std::sync::Arc;

use noted_db::chunks::PendingChunk;
use noted_db::PgPool;

use crate::provider::{validate_dimensions, verify_batch, EmbedError, EmbeddingProvider};

/// How many chunks to embed per round trip. Large enough to amortise the model
/// call, small enough that a crash loses little.
pub const BATCH_SIZE: i64 = 64;

/// How many consecutive failed batches `drain` tolerates before giving up.
///
/// A cap is what makes `drain` terminate. The queue is a set difference with no
/// state, so a chunk that cannot be embedded is returned by `pending()` forever
/// — without a cap, a broken provider would spin on it until the process is
/// killed, silently doing nothing. Any *successful* batch resets the count, so
/// this only fires when the worker has stopped making progress entirely.
pub const MAX_CONSECUTIVE_FAILURES: usize = 3;

/// The result of one batch.
///
/// `drain` cannot tell "no work left" from "this batch failed" if both are just
/// a count — the first must stop the loop, the second must not. Hence a distinct
/// outcome rather than a bare `usize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchOutcome {
    /// `n` chunks were embedded and stored. `Embedded(0)` means the queue is
    /// drained — the only clean stop.
    Embedded(usize),
    /// `n` chunks could not be embedded. They stay in the queue (there is
    /// nowhere else for them to go) and the drain moves on.
    Failed(usize),
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Embed(#[from] EmbedError),
    #[error(
        "embedding stalled: {batches} consecutive batches failed to embed (the last of \
         {chunks} chunk(s)); {embedded} chunk(s) were embedded before giving up. Either the \
         provider is broken, or those chunks cannot be embedded. Re-running is safe and will \
         retry them."
    )]
    Stalled { batches: usize, chunks: usize, embedded: usize },
}

pub struct Worker {
    pool: PgPool,
    provider: Arc<dyn EmbeddingProvider>,
}

impl Worker {
    /// Validates the provider against the schema's fixed dimension. A mismatched
    /// model is rejected HERE, before any work — accepting it would fill an
    /// unindexable column.
    pub fn new(pool: PgPool, provider: Arc<dyn EmbeddingProvider>) -> Result<Self, EmbedError> {
        validate_dimensions(provider.as_ref())?;
        Ok(Self { pool, provider })
    }

    /// Embed `texts`, checking the provider's output before it can reach a `zip`.
    ///
    /// `EmbeddingProvider::embed` is trait-object dispatched: the trait's contract
    /// does not force every implementation to call `provider::verify_batch` before
    /// returning (FastEmbed does, but that is an implementation detail of one impl,
    /// not a guarantee the trait makes). `zip` silently truncates to the shorter
    /// side on a length mismatch, which would pair chunk *i* with another chunk's
    /// vector — the exact silent-corruption path this codebase is built to avoid.
    /// Re-check here, at the one place every provider's output funnels through on
    /// its way into storage, so a future provider (e.g. Ollama, M1b-3) that forgets
    /// to call `verify_batch` internally still cannot reach `zip` unchecked.
    async fn embed_checked(
        &self,
        texts: &[String],
        model_id: &str,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let vectors = self.provider.embed(texts).await?;
        verify_batch(&vectors, texts.len(), model_id)?;
        Ok(vectors)
    }

    /// Retry a failed batch one chunk at a time, to find out which chunk is
    /// actually bad.
    ///
    /// Without this, failure isolation would only be batch-granular — and since
    /// `pending()` is deterministic, the poison chunk's batch would come back
    /// identical on every poll and block the up-to-63 healthy chunks sharing it,
    /// forever. Splitting the batch lets the good chunks through and narrows the
    /// blast radius to the one chunk that genuinely cannot be embedded.
    ///
    /// Only reached after a batch has already failed, so the extra round trips
    /// cost nothing on the happy path.
    async fn embed_individually(
        &self,
        batch: &[PendingChunk],
        model_id: &str,
    ) -> Vec<(String, Vec<f32>)> {
        let mut out = Vec::new();
        for chunk in batch {
            match self.embed_checked(std::slice::from_ref(&chunk.text), model_id).await {
                Ok(mut vectors) if !vectors.is_empty() => {
                    out.push((chunk.content_hash.clone(), vectors.remove(0)));
                }
                Ok(_) => {
                    tracing::warn!(
                        hash = %chunk.content_hash,
                        "provider returned no vector for a single chunk; skipping it"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        hash = %chunk.content_hash,
                        error = %e,
                        "chunk could not be embedded; skipping it and leaving it in the queue"
                    );
                }
            }
        }
        out
    }

    /// Embed one batch.
    ///
    /// An embedding failure does NOT abort: a single pathological chunk must not
    /// stop the whole backfill on every restart forever. A *storage* failure still
    /// propagates — a dead database is not a poison chunk, and retrying past it
    /// would just lose work.
    pub async fn run_once(&self) -> Result<BatchOutcome, WorkerError> {
        let model_id = self.provider.model_id().to_string();
        let batch = noted_db::chunks::pending(&self.pool, &model_id, BATCH_SIZE).await?;
        if batch.is_empty() {
            return Ok(BatchOutcome::Embedded(0));
        }

        let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();

        let rows: Vec<(String, Vec<f32>)> = match self.embed_checked(&texts, &model_id).await {
            Ok(vectors) => batch
                .iter()
                .map(|c| c.content_hash.clone())
                .zip(vectors)
                .collect(),
            Err(e) => {
                tracing::warn!(
                    batch = batch.len(),
                    error = %e,
                    "embedding batch failed; retrying chunk by chunk to isolate the bad one"
                );
                // A one-chunk batch has already been isolated as far as it can be:
                // splitting it again would just repeat the identical call.
                if batch.len() == 1 {
                    return Ok(BatchOutcome::Failed(1));
                }
                let salvaged = self.embed_individually(&batch, &model_id).await;
                if salvaged.is_empty() {
                    return Ok(BatchOutcome::Failed(batch.len()));
                }
                salvaged
            }
        };

        noted_db::chunks::store_embeddings_batch(&self.pool, &model_id, &rows).await?;

        let (done, total) = noted_db::chunks::progress(&self.pool, &model_id, None).await?;
        tracing::info!(embedded = done, total, "indexing progress");

        Ok(BatchOutcome::Embedded(rows.len()))
    }

    /// Run batches until the queue is empty. Safe to kill at any moment: the
    /// queue is a set difference with no in-progress state, so the next run
    /// picks up exactly what is left.
    ///
    /// Terminates on one of three things: an empty queue, a database error, or
    /// `MAX_CONSECUTIVE_FAILURES` batches in a row that embedded nothing.
    pub async fn drain(&self) -> Result<usize, WorkerError> {
        let mut total = 0;
        let mut consecutive_failures = 0usize;
        loop {
            match self.run_once().await? {
                BatchOutcome::Embedded(0) => return Ok(total),
                BatchOutcome::Embedded(n) => {
                    total += n;
                    // Progress was made, so whatever failed before was not fatal.
                    consecutive_failures = 0;
                }
                BatchOutcome::Failed(n) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        return Err(WorkerError::Stalled {
                            batches: consecutive_failures,
                            chunks: n,
                            embedded: total,
                        });
                    }
                }
            }
        }
    }
}

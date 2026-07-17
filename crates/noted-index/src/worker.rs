use std::sync::Arc;

use noted_db::PgPool;

use crate::provider::{validate_dimensions, verify_batch, EmbedError, EmbeddingProvider};

/// How many chunks to embed per round trip. Large enough to amortise the model
/// call, small enough that a crash loses little.
pub const BATCH_SIZE: i64 = 64;

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Embed(#[from] EmbedError),
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

    /// Embed one batch. Returns how many chunks were embedded; 0 means the queue
    /// is drained.
    pub async fn run_once(&self) -> Result<usize, WorkerError> {
        let model_id = self.provider.model_id().to_string();
        let batch = noted_db::chunks::pending(&self.pool, &model_id, BATCH_SIZE).await?;
        if batch.is_empty() {
            return Ok(0);
        }

        let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
        let vectors = self.provider.embed(&texts).await?;

        // `EmbeddingProvider::embed` is trait-object dispatched: the trait's
        // contract does not force every implementation to call
        // `provider::verify_batch` before returning (FastEmbed does, but that is
        // an implementation detail of one impl, not a guarantee the trait makes).
        // The `zip` below silently truncates to the shorter side on a length
        // mismatch, which would pair chunk *i* with another chunk's vector — the
        // exact silent-corruption path this codebase is built to avoid. Re-check
        // here, at the one place every provider's output funnels through on its
        // way into storage, so a future provider (e.g. Ollama, M1b-3) that
        // forgets to call `verify_batch` internally still cannot reach `zip`
        // unchecked.
        verify_batch(&vectors, batch.len(), &model_id)?;

        for (chunk, vector) in batch.iter().zip(vectors.iter()) {
            noted_db::chunks::store_embedding(
                &self.pool,
                &chunk.content_hash,
                &model_id,
                vector,
            )
            .await?;
        }

        let (done, total) = noted_db::chunks::progress(&self.pool, &model_id, None).await?;
        tracing::info!(embedded = done, total, "indexing progress");

        Ok(batch.len())
    }

    /// Run batches until the queue is empty. Safe to kill at any moment: the
    /// queue is a set difference with no in-progress state, so the next run
    /// picks up exactly what is left.
    pub async fn drain(&self) -> Result<usize, WorkerError> {
        let mut total = 0;
        loop {
            let n = self.run_once().await?;
            if n == 0 {
                return Ok(total);
            }
            total += n;
        }
    }
}

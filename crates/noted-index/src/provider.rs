/// bge-base-en-v1.5. Baked into the `vector(768)` column because pgvector cannot
/// index a dimensionless vector — so changing the model is a migration plus a
/// full re-embed, deliberately. See the M1b spec §5.2.
pub const EMBEDDING_DIMS: usize = 768;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("embedding model error: {0}")]
    Model(String),
    #[error(
        "embedding model '{model}' produces {got}-dimensional vectors, but the schema \
         column is vector({expected}). Change the model, or migrate the column and \
         re-embed."
    )]
    DimensionMismatch {
        expected: usize,
        got: usize,
        model: String,
    },
    #[error("embedding model '{model}' returned {got} vectors for {expected} inputs")]
    BatchSizeMismatch {
        expected: usize,
        got: usize,
        model: String,
    },
}

/// Shared invariant check for any `EmbeddingProvider::embed` implementation: the
/// provider must return exactly one vector per input, each of `EMBEDDING_DIMS`
/// length. `zip` silently truncates on a length mismatch, so without this check a
/// short batch would pair chunk *i* with another chunk's embedding — a data
/// corruption path that surfaces only as confidently wrong search results.
/// `pub` (not `pub(crate)`) so future providers (e.g. Task M1b-3's Ollama
/// provider) reuse it, and so the integration tests in `tests/provider.rs` —
/// which compile as a separate crate and cannot see `pub(crate)` items — can
/// exercise it directly.
pub fn verify_batch(out: &[Vec<f32>], expected: usize, model: &str) -> Result<(), EmbedError> {
    if out.len() != expected {
        return Err(EmbedError::BatchSizeMismatch {
            expected,
            got: out.len(),
            model: model.to_string(),
        });
    }
    if let Some(bad) = out.iter().find(|v| v.len() != EMBEDDING_DIMS) {
        return Err(EmbedError::DimensionMismatch {
            expected: EMBEDDING_DIMS,
            got: bad.len(),
            model: model.to_string(),
        });
    }
    Ok(())
}

#[async_trait::async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn dimensions(&self) -> usize;
    fn model_id(&self) -> &str;
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// Call this at startup, before any work. Rejecting a mismatched model here is a
/// correctness requirement: accepting it produces a column no index can cover.
pub fn validate_dimensions(p: &dyn EmbeddingProvider) -> Result<(), EmbedError> {
    if p.dimensions() != EMBEDDING_DIMS {
        return Err(EmbedError::DimensionMismatch {
            expected: EMBEDDING_DIMS,
            got: p.dimensions(),
            model: p.model_id().to_string(),
        });
    }
    Ok(())
}

/// Local ONNX embeddings. No Python, no Ollama, no API key — this is what keeps
/// "the app runs with only Postgres" true.
pub struct FastEmbed {
    model: std::sync::Arc<std::sync::Mutex<fastembed::TextEmbedding>>,
}

impl FastEmbed {
    pub fn new() -> Result<Self, EmbedError> {
        let model = fastembed::TextEmbedding::try_new(
            fastembed::InitOptions::new(fastembed::EmbeddingModel::BGEBaseENV15)
                .with_show_download_progress(true),
        )
        .map_err(|e| EmbedError::Model(e.to_string()))?;
        let provider = Self {
            model: std::sync::Arc::new(std::sync::Mutex::new(model)),
        };
        // A `FastEmbed` must not be able to exist in a state that violates the
        // schema's dimensionality. `validate_dimensions` stays public too — Task 6
        // still calls it for arbitrary `dyn EmbeddingProvider`s (Task M1b-3 adds an
        // Ollama provider whose dimensions are only known at runtime), so this
        // self-check is defence in depth, not a replacement for that call.
        validate_dimensions(&provider)?;
        Ok(provider)
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for FastEmbed {
    fn dimensions(&self) -> usize {
        EMBEDDING_DIMS
    }
    fn model_id(&self) -> &str {
        "bge-base-en-v1.5"
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let n = texts.len();
        if n == 0 {
            return Ok(Vec::new());
        }
        let model = std::sync::Arc::clone(&self.model);
        let owned: Vec<String> = texts.to_vec();

        // ONNX inference is synchronous and CPU-bound (seconds per batch, no GPU
        // on this machine). Running it inline would block a tokio worker thread
        // for the whole call and stall every other task during a backfill.
        let out: Vec<Vec<f32>> = tokio::task::spawn_blocking(move || {
            let mut m = model
                .lock()
                .map_err(|e| EmbedError::Model(format!("embedding model mutex poisoned: {e}")))?;
            m.embed(owned, None)
                .map_err(|e| EmbedError::Model(e.to_string()))
        })
        .await
        .map_err(|e| EmbedError::Model(format!("embedding task failed: {e}")))??;

        verify_batch(&out, n, self.model_id())?;
        Ok(out)
    }
}

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
    DimensionMismatch { expected: usize, got: usize, model: String },
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
    model: tokio::sync::Mutex<fastembed::TextEmbedding>,
}

impl FastEmbed {
    pub fn new() -> Result<Self, EmbedError> {
        let model = fastembed::TextEmbedding::try_new(
            fastembed::InitOptions::new(fastembed::EmbeddingModel::BGEBaseENV15)
                .with_show_download_progress(true),
        )
        .map_err(|e| EmbedError::Model(e.to_string()))?;
        Ok(Self { model: tokio::sync::Mutex::new(model) })
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for FastEmbed {
    fn dimensions(&self) -> usize { EMBEDDING_DIMS }
    fn model_id(&self) -> &str { "bge-base-en-v1.5" }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut m = self.model.lock().await;
        m.embed(texts.to_vec(), None)
            .map_err(|e| EmbedError::Model(e.to_string()))
    }
}

use noted_index::provider::{
    validate_dimensions, verify_batch, EmbedError, EmbeddingProvider, EMBEDDING_DIMS,
};

struct Fake(usize);

#[async_trait::async_trait]
impl EmbeddingProvider for Fake {
    fn dimensions(&self) -> usize { self.0 }
    fn model_id(&self) -> &str { "fake" }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.1; self.0]).collect())
    }
}

#[test]
fn a_provider_matching_the_schema_dimension_is_accepted() {
    assert!(validate_dimensions(&Fake(EMBEDDING_DIMS)).is_ok());
}

/// The schema column is vector(768) and pgvector cannot index a dimensionless
/// vector. A mismatched provider must be REJECTED, loudly, at startup — silently
/// accepting one yields an unindexable column that presents to the user as
/// "search got slow" and is nearly undiagnosable.
#[test]
fn a_mismatched_provider_is_rejected_with_a_useful_error() {
    let err = validate_dimensions(&Fake(1024)).unwrap_err();
    match err {
        EmbedError::DimensionMismatch { expected, got, ref model } => {
            assert_eq!(expected, EMBEDDING_DIMS);
            assert_eq!(got, 1024);
            assert_eq!(model, "fake");
        }
        other => panic!("expected DimensionMismatch, got {other:?}"),
    }
    let msg = validate_dimensions(&Fake(1024)).unwrap_err().to_string();
    assert!(msg.contains("768") && msg.contains("1024"), "error must name both dimensions: {msg}");
}

/// `zip` silently truncates on a length mismatch, so a provider that returns
/// fewer vectors than inputs must be rejected loudly rather than let the caller
/// pair chunk *i* with another chunk's embedding. `verify_batch` is the shared
/// helper `FastEmbed::embed` calls before returning; tested directly here since
/// it is `pub` for exactly this reason (see its doc comment in provider.rs).
#[test]
fn a_provider_returning_too_few_vectors_is_an_error() {
    let texts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let out: Vec<Vec<f32>> = (0..texts.len() - 1).map(|_| vec![0.1; EMBEDDING_DIMS]).collect();

    let err = verify_batch(&out, texts.len(), "fake").unwrap_err();
    match err {
        EmbedError::BatchSizeMismatch { expected, got, ref model } => {
            assert_eq!(expected, 3);
            assert_eq!(got, 2);
            assert_eq!(model, "fake");
        }
        other => panic!("expected BatchSizeMismatch, got {other:?}"),
    }
}

#[test]
fn a_provider_returning_wrong_dimensions_is_an_error() {
    let out = vec![vec![0.1; 512], vec![0.1; 512]];

    let err = verify_batch(&out, 2, "fake").unwrap_err();
    match err {
        EmbedError::DimensionMismatch { expected, got, ref model } => {
            assert_eq!(expected, EMBEDDING_DIMS);
            assert_eq!(got, 512);
            assert_eq!(model, "fake");
        }
        other => panic!("expected DimensionMismatch, got {other:?}"),
    }
    let msg = verify_batch(&out, 2, "fake").unwrap_err().to_string();
    assert!(msg.contains("768") && msg.contains("512"), "error must name both dimensions: {msg}");
}

/// `FastEmbed::embed` short-circuits on an empty batch before touching the model
/// at all (no `spawn_blocking`, no lock). `Fake` here stands in as any correct
/// `EmbeddingProvider`: an empty input must yield `Ok(vec![])` and nothing else,
/// which `verify_batch` also agrees with (0 expected, 0 got).
#[tokio::test]
async fn an_empty_batch_is_ok_and_calls_nothing() {
    let p = Fake(EMBEDDING_DIMS);
    let out = p.embed(&[]).await.unwrap();
    assert_eq!(out, Vec::<Vec<f32>>::new());
    assert!(verify_batch(&out, 0, "fake").is_ok());
}

/// Slow: downloads the ONNX model on first run. Ignored by default; this is the
/// only test that proves the real provider works.
#[tokio::test]
#[ignore]
async fn fastembed_produces_768_dimensional_vectors() {
    let p = noted_index::provider::FastEmbed::new().expect("model load");
    assert_eq!(p.dimensions(), EMBEDDING_DIMS);
    validate_dimensions(&p).expect("the default provider must satisfy the schema");

    let out = p.embed(&["hello world".to_string(), "goodbye".to_string()]).await.unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].len(), EMBEDDING_DIMS);
    assert!(out[0].iter().any(|v| *v != 0.0), "embedding must not be all zeros");
    assert_ne!(out[0], out[1], "different text must embed differently");
}

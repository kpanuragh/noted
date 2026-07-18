#[derive(Debug, Clone)]
pub struct ExtractedEntity {
    pub name: String,
    pub entity_type: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExtractedEdge {
    pub source: String,
    pub target: String,
    pub relation: String,
    pub weight: f32,
}

#[derive(Debug, Clone, Default)]
pub struct Extraction {
    pub entities: Vec<ExtractedEntity>,
    pub edges: Vec<ExtractedEdge>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("extraction model error: {0}")]
    Model(String),
    #[error("extraction output invalid: {0}")]
    Invalid(String),
}

#[async_trait::async_trait]
pub trait ExtractionProvider: Send + Sync {
    fn model_id(&self) -> &str;
    async fn extract(&self, text: &str) -> Result<Extraction, ExtractError>;
}

/// Canonical form used as the per-workspace resolution key.
pub fn normalise_entity(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Deterministic, no-model extractor for tests. Treats each capitalised word as
/// an entity and links consecutive ones — so a test controls the graph exactly.
pub struct StubExtractor;

impl StubExtractor {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubExtractor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ExtractionProvider for StubExtractor {
    fn model_id(&self) -> &str {
        "stub-extractor-v1"
    }

    async fn extract(&self, text: &str) -> Result<Extraction, ExtractError> {
        let names: Vec<String> = text
            .split_whitespace()
            .filter(|w| w.chars().next().is_some_and(|c| c.is_uppercase()))
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
            .filter(|w| !w.is_empty())
            .collect();
        let entities = names
            .iter()
            .map(|n| ExtractedEntity {
                name: n.clone(),
                entity_type: "CONCEPT".into(),
                description: None,
            })
            .collect();
        let edges = names
            .windows(2)
            .map(|w| ExtractedEdge {
                source: w[0].clone(),
                target: w[1].clone(),
                relation: "mentions_with".into(),
                weight: 1.0,
            })
            .collect();
        Ok(Extraction { entities, edges })
    }
}

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
    /// The provider refused because the caller is over its rate or quota limit.
    ///
    /// Distinct from [`ExtractError::Model`] because the correct response is
    /// different in kind: a model error affects ONE chunk and the next one is
    /// worth trying, whereas a rate limit affects every call that would follow
    /// it. Collapsing the two turns a single 429 into a whole batch of doomed
    /// requests, each of which spends quota to be refused — observed against
    /// the live Gemini API, which retried four times in two seconds.
    #[error("extraction rate-limited: {0}")]
    RateLimited(String),
}

#[async_trait::async_trait]
pub trait ExtractionProvider: Send + Sync {
    fn model_id(&self) -> &str;
    async fn extract(&self, text: &str) -> Result<Extraction, ExtractError>;
}

/// Drops the parts of an extraction that cannot be written to a graph, and
/// trims the rest.
///
/// Every provider must run its output through this — it is not a nicety.
/// `normalise_entity` lowercases and collapses whitespace, so an entity with a
/// BLANK name normalises to `""`, and `resolve_entity` keys on that. Within a
/// workspace every blank-named entity from every chunk therefore collapses onto
/// a SINGLE node, which then accumulates an edge from every chunk that produced
/// one — a false hub joining unrelated things, and the sort of structure
/// Louvain will happily build a community around.
///
/// This is not defensive programming against a hypothetical. `llama3.2:1b`
/// does it readily: asked to extract from a meeting note it returned entities
/// with an empty `name` and the note's topic in `entity_type`, having simply
/// swapped the fields. The Gemini provider filtered these from the start; the
/// Ollama provider did not, because until now nothing had ever run it against
/// a real model.
pub fn sanitise(extraction: Extraction) -> Extraction {
    Extraction {
        entities: extraction
            .entities
            .into_iter()
            .filter(|e| !e.name.trim().is_empty())
            .map(|e| ExtractedEntity {
                name: e.name.trim().to_string(),
                entity_type: e.entity_type.trim().to_string(),
                // A whitespace-only description is not a description, and
                // storing one costs a row that reads as "described" downstream.
                description: e.description.filter(|d| !d.trim().is_empty()),
            })
            .collect(),
        edges: extraction
            .edges
            .into_iter()
            .filter(|e| !e.source.trim().is_empty() && !e.target.trim().is_empty())
            .map(|e| ExtractedEdge {
                source: e.source.trim().to_string(),
                target: e.target.trim().to_string(),
                relation: e.relation.trim().to_string(),
                weight: e.weight,
            })
            .collect(),
    }
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

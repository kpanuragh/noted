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
            // `is_plausible_entity_name` subsumes the old blank check and adds
            // the noise rules — see its docs. A node that should not exist is
            // dropped here, before it can resolve and accrete edges.
            .filter(|e| is_plausible_entity_name(&e.name))
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
            // An edge is only as trustworthy as its endpoints. If either end is
            // noise the edge is dropped too — a relationship to a node that
            // should not exist is not a relationship. Uses the SAME predicate
            // as the entity filter, so the two can never disagree about what
            // counts as a real endpoint.
            .filter(|e| is_plausible_entity_name(&e.source) && is_plausible_entity_name(&e.target))
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
    let collapsed = name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    // Strip punctuation that clings to a name when a model copies it out of
    // prose: `(kerala)`, `kerala.`, `"kerala"`. Only from the ENDS, so a
    // hyphenated or apostrophed interior is untouched.
    let trimmed = collapsed
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_string();
    // Drop a trailing possessive so `india's` resolves to the same node as
    // `india`. The possessive ONLY — not every trailing `s` — or `ghats` would
    // silently become `ghat` and stop matching itself.
    trimmed
        .strip_suffix("'s")
        .or_else(|| trimmed.strip_suffix('\''))
        .unwrap_or(&trimmed)
        .to_string()
}

/// The extraction instructions, shared by every real provider.
///
/// One definition, because a graph built by two providers under two prompts is
/// two different graphs. It was two — the Ollama and Gemini providers each
/// carried their own loosely-worded preamble, and the loose wording is most of
/// why `llama3.2:1b` returned sentence fragments and stopwords as entities.
///
/// The constraints below are written AT the failure modes seen in real output:
/// clauses-as-entities, possessives as separate nodes, pronouns and articles,
/// and edges to things that were never entities. A better model needs less of
/// this, but the instructions cost nothing and make a weak model usable.
pub const EXTRACTION_INSTRUCTIONS: &str = "\
You are an information-extraction engine building a knowledge graph. Read the \
note below and extract its entities and the relationships between them.

An ENTITY is a specific, nameable thing: a person, place, organisation, named \
concept, event, or work. Give each a short canonical name (1 to 4 words) and a \
type. Follow these rules exactly:
- Use the base form of the name. \"India\", never \"India's\". Prefer the \
singular unless the plural IS the thing.
- A name is a noun, never a phrase, clause, sentence, or list. If you are \
tempted to include a comma, it is not one entity.
- Do NOT extract pronouns, articles, or generic words (it, they, thing, area, \
region) — only things worth their own node.
- Extract only what the text states. Do not infer or invent.

A RELATIONSHIP connects two entities you extracted, names how they relate in a \
few words, and carries a confidence weight from 0.0 to 1.0. Every endpoint must \
be one of the entities above.

Respond with ONLY the JSON object the schema describes — no prose, no markdown \
fences. If the note contains nothing extractable, return empty arrays rather \
than inventing content.

Note:
";

/// The full extraction prompt for `text`, instructions plus the note.
///
/// Shared so both providers send byte-identical prompts and cannot drift.
pub fn build_extraction_prompt(text: &str) -> String {
    format!("{EXTRACTION_INSTRUCTIONS}{text}")
}

/// The longest an entity NAME may be, in words and characters.
///
/// A weak extractor (`llama3.2:1b`) readily returns whole clauses as
/// "entities" — `palm-lined beaches and backwaters, a network of canals.` came
/// back as one. A graph node is a thing, not a sentence, so these bounds trade
/// a little recall for nodes that are actually entities. Chosen from the real
/// output: legitimate names in a personal corpus (`Western Ghats`, `Sir Richard
/// Owen`) sit well under both.
const MAX_ENTITY_WORDS: usize = 6;
const MAX_ENTITY_CHARS: usize = 64;

/// Whether a string is plausibly an entity NAME rather than extraction noise.
///
/// Applied by [`sanitise`] to both entities and edge endpoints. Every reject
/// rule here fires on real `llama3.2:1b` output; none fires on the legitimate
/// entities from the same runs. The rules, and why each is safe:
///
///   * **Empty / no letter** — `123`, `---`, `   ` are not names.
///   * **A comma or semicolon** — the model handed back a LIST or a clause
///     (`national parks, plus wayanad and other sanctuaries`), not one name.
///   * **Over-long** — past [`MAX_ENTITY_WORDS`]/[`MAX_ENTITY_CHARS`] it is a
///     phrase. A real multi-word entity is short.
pub fn is_plausible_entity_name(name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    if !name.chars().any(|c| c.is_alphabetic()) {
        return false;
    }
    if name.contains(',') || name.contains(';') {
        return false;
    }
    if name.chars().count() > MAX_ENTITY_CHARS {
        return false;
    }
    if name.split_whitespace().count() > MAX_ENTITY_WORDS {
        return false;
    }
    true
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

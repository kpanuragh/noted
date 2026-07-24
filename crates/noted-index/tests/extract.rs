use noted_index::extract::{ExtractionProvider, StubExtractor, normalise_entity};

#[test]
fn normalise_collapses_case_and_whitespace() {
    assert_eq!(normalise_entity("  PostgreSQL "), "postgresql");
    assert_eq!(normalise_entity("Query   Planner"), "query planner");
    assert_eq!(normalise_entity("postgres"), "postgres");
}

#[tokio::test]
async fn stub_extraction_is_deterministic_and_text_derived() {
    let s = StubExtractor::new();
    let a = s.extract("Postgres uses MVCC").await.unwrap();
    let b = s.extract("Postgres uses MVCC").await.unwrap();
    // Deterministic: same text -> same graph.
    let names_a: Vec<_> = a.entities.iter().map(|e| e.name.clone()).collect();
    let names_b: Vec<_> = b.entities.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names_a, names_b);
    // Text-derived: capitalised words become entities.
    assert!(a.entities.iter().any(|e| e.name == "Postgres"));
    assert!(a.entities.iter().any(|e| e.name == "MVCC"));
    // An edge connects them so tests can assert graph shape.
    assert!(
        !a.edges.is_empty(),
        "stub must produce at least one edge for multi-entity text"
    );
}

#[tokio::test]
async fn stub_model_id_is_stable() {
    assert_eq!(
        StubExtractor::new().model_id(),
        StubExtractor::new().model_id()
    );
}

/// A blank entity name is graph CORRUPTION, not noise, and every provider must
/// drop it before it reaches `graph_write`.
///
/// `normalise_entity` collapses whitespace and lowercases, so a blank name
/// normalises to `""` and `resolve_entity` keys on that. Within a workspace
/// every blank-named entity from every chunk therefore resolves to ONE node,
/// which accumulates an edge from each chunk that produced one — a false hub
/// joining unrelated notes, which Louvain will then build a community around.
///
/// Found by running `llama3.2:1b` for real: it returned entities with an empty
/// `name` and the note's topic in `entity_type`, having swapped the fields.
#[test]
fn sanitise_drops_blank_names_before_they_become_a_false_hub() {
    use noted_index::extract::{sanitise, ExtractedEdge, ExtractedEntity, Extraction};

    let out = sanitise(Extraction {
        entities: vec![
            ExtractedEntity {
                name: "   ".into(),
                entity_type: "quarterly_planning_meeting".into(),
                description: None,
            },
            ExtractedEntity {
                name: "  Priya Raman  ".into(),
                entity_type: " person ".into(),
                description: Some("   ".into()),
            },
        ],
        edges: vec![
            // Both endpoints must survive; a blank one cannot be resolved to
            // anything meaningful.
            ExtractedEdge {
                source: "Priya Raman".into(),
                target: "".into(),
                relation: "owns".into(),
                weight: 0.9,
            },
            ExtractedEdge {
                source: " Priya Raman ".into(),
                target: " Arun ".into(),
                relation: " reports to ".into(),
                weight: 0.8,
            },
        ],
    });

    assert_eq!(out.entities.len(), 1, "the blank-named entity survived");
    assert_eq!(out.entities[0].name, "Priya Raman", "name was not trimmed");
    assert_eq!(out.entities[0].entity_type, "person");
    assert_eq!(
        out.entities[0].description, None,
        "a whitespace-only description is not a description"
    );

    assert_eq!(out.edges.len(), 1, "an edge with a blank endpoint survived");
    assert_eq!(out.edges[0].source, "Priya Raman");
    assert_eq!(out.edges[0].target, "Arun");
    assert_eq!(out.edges[0].relation, "reports to");
}

// -------------------------------------------------------- entity quality --
//
// These pin the noise filter and normalisation that a weak extraction model
// (llama3.2:1b) makes necessary. Every example below is a REAL entity the 1b
// model produced from a two-paragraph note about Kerala and dinosaurs.

use noted_index::extract::is_plausible_entity_name;

#[test]
fn a_sentence_fragment_is_not_an_entity() {
    // The model returned these verbatim as "entities". A knowledge graph node
    // is a thing, not a clause.
    assert!(!is_plausible_entity_name(
        "palm-lined beaches and backwaters, a network of canals."
    ));
    assert!(!is_plausible_entity_name(
        "national parks, plus wayanad and other sanctuaries"
    ));
    // A comma or semicolon means the model handed back a list, not a name.
    assert!(!is_plausible_entity_name("apples, oranges"));
}

#[test]
fn an_over_long_phrase_is_not_an_entity() {
    // No entity name is a seven-word phrase. Bounds recall slightly in exchange
    // for a graph whose nodes are actually entities.
    assert!(!is_plausible_entity_name("the quick brown fox jumped over something"));
}

#[test]
fn punctuation_or_number_only_is_not_an_entity() {
    assert!(!is_plausible_entity_name("123"));
    assert!(!is_plausible_entity_name("---"));
    assert!(!is_plausible_entity_name("   "));
}

#[test]
fn real_multiword_entities_are_kept() {
    // The filter must not be so aggressive it eats legitimate names.
    for good in [
        "Western Ghats",
        "Sir Richard Owen",
        "great fossil lizards",
        "Kerala",
        "U.S.A.",
        "dinosaurs",
    ] {
        assert!(is_plausible_entity_name(good), "wrongly rejected: {good}");
    }
}

#[test]
fn normalise_strips_a_possessive_so_the_forms_merge() {
    // "India" and "India's" are the same entity; the graph should hold one node.
    assert_eq!(normalise_entity("India's"), normalise_entity("India"));
    assert_eq!(normalise_entity("India's"), "india");
    // But a plain trailing s is NOT a possessive and must be left alone, or
    // "Ghats" would silently become "ghat".
    assert_eq!(normalise_entity("Ghats"), "ghats");
}

#[test]
fn normalise_strips_clinging_punctuation() {
    // Punctuation a model copies out of prose alongside the word.
    assert_eq!(normalise_entity("(Kerala)"), "kerala");
    assert_eq!(normalise_entity("Kerala."), "kerala");
    assert_eq!(normalise_entity("\"Kerala\""), "kerala");
    // ...without disturbing the case+whitespace collapse it always did.
    assert_eq!(normalise_entity("  Western   GHATS  "), "western ghats");
}

#[test]
fn sanitise_drops_noise_entities_and_edges_that_reference_them() {
    use noted_index::extract::{sanitise, ExtractedEdge, ExtractedEntity, Extraction};

    let out = sanitise(Extraction {
        entities: vec![
            ExtractedEntity { name: "Kerala".into(), entity_type: "place".into(), description: None },
            ExtractedEntity {
                name: "palm-lined beaches and backwaters, a network of canals.".into(),
                entity_type: "concept".into(),
                description: None,
            },
        ],
        edges: vec![
            // An edge to the noise entity must go too — a node that should not
            // exist cannot have real relationships.
            ExtractedEdge {
                source: "Kerala".into(),
                target: "palm-lined beaches and backwaters, a network of canals.".into(),
                relation: "has".into(),
                weight: 0.5,
            },
            ExtractedEdge {
                source: "Kerala".into(),
                target: "Western Ghats".into(),
                relation: "borders".into(),
                weight: 0.9,
            },
        ],
    });

    let names: Vec<&str> = out.entities.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["Kerala"], "the sentence-fragment entity survived");
    assert_eq!(out.edges.len(), 1, "the edge to the noise entity survived");
    assert_eq!(out.edges[0].target, "Western Ghats");
}

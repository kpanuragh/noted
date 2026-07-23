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

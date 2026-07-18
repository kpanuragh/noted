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

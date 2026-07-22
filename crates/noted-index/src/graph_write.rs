//! Bridges `noted-index`'s `Extraction` type (entities + edges by NAME, as an
//! extractor emits them) to `noted-db::graph`'s primitive, id-based writes.
//!
//! This lives here rather than in `noted-db` because `noted-db` must not
//! depend back on `noted-index` (see `noted_db::graph`'s module docs) — the
//! `Extraction` -> resolved-rows mapping has to live on this side of that
//! boundary.
use crate::extract::{Extraction, normalise_entity};
use noted_db::graph;
use std::collections::HashMap;
use uuid::Uuid;

/// Apply one chunk's extraction result to the graph: resolve every entity to
/// an id, resolve every edge's endpoints to ids, and replace that chunk's
/// edge set under `model_id`.
///
/// Entity resolution: every `ExtractedEntity` is normalised
/// (`normalise_entity`) and resolved via `graph::resolve_entity`, building a
/// normalised-name -> id map.
///
/// Edge endpoint resolution: an edge's source/target is normally one of the
/// entities already extracted alongside it, so the map built above already
/// has it. But an extractor is free to emit an edge whose endpoint name
/// didn't also appear in `ex.entities` (a sloppy provider, or a future
/// LLM-backed one that under-lists entities relative to edges) — DECISION:
/// resolve it too, on demand, rather than silently dropping the edge. It is
/// resolved with an UNKNOWN entity type (`None`) and no description, rather
/// than dropping the edge. Passing `None` — not `Some("CONCEPT")` — is
/// load-bearing: `resolve_entity` treats an explicit type as a
/// reclassification that overwrites, so claiming "CONCEPT" here would let a
/// passing edge-endpoint mention DOWNGRADE an entity a previous pass had
/// correctly typed as `PERSON`/`ORG`. `None` says "exists, type unknown",
/// which inserts the `CONCEPT` default for a brand-new node and leaves an
/// existing node's type alone. Dropping the edge instead would silently lose
/// provenance the extractor clearly intended to record; resolving is the more
/// conservative choice.
/// What one `apply_extraction` call actually changed.
///
/// Returned rather than discarded because the community layer needs BOTH halves
/// and can derive neither: `edges` is the churn to report, and `entities` is the
/// set to hand `hot_reassign`.
///
/// **`entities` is the edge ENDPOINTS, not every entity resolved.** An entity
/// that this extraction named but connected to nothing has no neighbour to join,
/// so reassigning it is a no-op. And passing more than an edit actually touched
/// is actively harmful: `hot_reassign` cascades transitively, so handing it a
/// broad set merges communities that should have stayed distinct.
///
/// KNOWN GAP, deliberate: an entity that this chunk PREVIOUSLY connected and no
/// longer does is not in this set — `replace_chunk_edges` deletes the old edges
/// inside its own transaction and does not report what it removed. Such an
/// entity keeps its old community until the next cold run corrects it, which is
/// exactly the bounded, self-correcting approximation the hot/cold design is
/// built on (M2b design §2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionApplied {
    /// Deduped edge endpoints, first-seen order.
    pub entities: Vec<Uuid>,
    /// How many edges were written — the churn to report.
    pub edges: i64,
}

pub async fn apply_extraction(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    chunk_hash: &str,
    model_id: &str,
    ex: &Extraction,
) -> Result<ExtractionApplied, sqlx::Error> {
    let mut ids: HashMap<String, Uuid> = HashMap::with_capacity(ex.entities.len());

    for entity in &ex.entities {
        let normalised = normalise_entity(&entity.name);
        let id = graph::resolve_entity(
            pool,
            workspace_id,
            &normalised,
            Some(entity.entity_type.as_str()),
            entity.description.as_deref(),
        )
        .await?;
        ids.insert(normalised, id);
    }

    let mut tuples: Vec<(Uuid, Uuid, String, f32)> = Vec::with_capacity(ex.edges.len());
    for edge in &ex.edges {
        let source_name = normalise_entity(&edge.source);
        let target_name = normalise_entity(&edge.target);

        let source_id = match ids.get(&source_name) {
            Some(id) => *id,
            None => {
                let id =
                    graph::resolve_entity(pool, workspace_id, &source_name, None, None).await?;
                ids.insert(source_name.clone(), id);
                id
            }
        };
        let target_id = match ids.get(&target_name) {
            Some(id) => *id,
            None => {
                let id =
                    graph::resolve_entity(pool, workspace_id, &target_name, None, None).await?;
                ids.insert(target_name.clone(), id);
                id
            }
        };

        tuples.push((source_id, target_id, edge.relation.clone(), edge.weight));
    }

    graph::replace_chunk_edges(pool, workspace_id, chunk_hash, model_id, &tuples).await?;

    // Deduped endpoints, in first-seen order so the result is deterministic for
    // a deterministic extractor — `hot_reassign` applies moves IN SEQUENCE, each
    // reading the last one's state, so the order is part of the outcome.
    let mut affected: Vec<Uuid> = Vec::with_capacity(tuples.len() * 2);
    for (source, target, _, _) in &tuples {
        if !affected.contains(source) {
            affected.push(*source);
        }
        if !affected.contains(target) {
            affected.push(*target);
        }
    }

    Ok(ExtractionApplied {
        edges: tuples.len() as i64,
        entities: affected,
    })
}

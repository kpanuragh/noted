//! Projecting typed property values into the knowledge graph.
//!
//! # Why structured values belong in the graph at all
//!
//! A select value or a relation is a fact a PERSON asserted, not one a model
//! inferred from prose. It is the strongest signal in the workspace, and until
//! now the graph could not see it — the graph was built entirely from text an
//! extractor read, so "this task is blocked by that one" existed in a database
//! column and nowhere in the knowledge graph.
//!
//! # Provenance is kept distinct, deliberately
//!
//! These edges live in their own table rather than in `edges`, because
//! `edges.source_chunk_hash` is NOT NULL and references a real chunk. A
//! property-derived edge has no chunk, and inventing a fake one to fit would
//! weaken a constraint that has caught real bugs. Readers union the two; the
//! chunk-provenance invariant stays true of every row in `edges`.
use uuid::Uuid;

use crate::collections::PropertyKind;

/// Rebuild every property-derived edge for one page.
///
/// Replace-not-append, in one transaction, exactly like `replace_chunk_edges`:
/// a page's property edges are wholly determined by its current values, so the
/// old set must go rather than accumulate. Editing a select from "blocked" to
/// "done" must not leave the page asserting both.
///
/// Returns how many edges the page now contributes.
pub async fn reproject_page(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    page_id: Uuid,
) -> Result<usize, sqlx::Error> {
    // Values the page holds, with the kind and name of each property.
    //
    // TWO queries, not one, because a relation's value is NOT in
    // `page_properties` — links are rows in `page_relations` (see the M3-4
    // migration for why). A single join over `page_properties` therefore finds
    // every kind EXCEPT relations, which is exactly the bug the first version
    // of this shipped: relation edges silently never appeared.
    let mut rows: Vec<(Uuid, String, String, serde_json::Value)> = sqlx::query_as(
        "SELECT cp.id, cp.name, cp.kind, pp.value
         FROM page_properties pp
         JOIN collection_properties cp ON cp.id = pp.property_id
         WHERE pp.page_id = $1 AND pp.value <> 'null'::jsonb",
    )
    .bind(page_id)
    .fetch_all(pool)
    .await?;

    let relation_props: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT DISTINCT cp.id, cp.name
         FROM page_relations r
         JOIN collection_properties cp ON cp.id = r.property_id
         WHERE r.from_page = $1",
    )
    .bind(page_id)
    .fetch_all(pool)
    .await?;
    for (id, name) in relation_props {
        rows.push((id, name, "relation".to_string(), serde_json::Value::Null));
    }

    // The page's own title names the entity the page IS. Everything a page
    // asserts is an edge FROM itself, so without this there is nothing to
    // attach the values to.
    let title: Option<String> =
        sqlx::query_scalar("SELECT title FROM pages WHERE id = $1 AND archived_at IS NULL")
            .bind(page_id)
            .fetch_optional(pool)
            .await?;

    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM property_edges WHERE page_id = $1")
        .bind(page_id)
        .execute(&mut *tx)
        .await?;

    // An archived page contributes nothing — the delete above is the whole job.
    let Some(title) = title else {
        tx.commit().await?;
        return Ok(0);
    };

    let subject = crate::graph::resolve_entity_tx(
        &mut tx,
        workspace_id,
        &crate::graph::normalise_for_graph(&title),
        Some("PAGE"),
        None,
    )
    .await?;

    let mut written = 0usize;
    for (property_id, prop_name, kind_str, value) in rows {
        let Some(kind) = PropertyKind::parse(&kind_str) else {
            continue;
        };

        // Which values are worth a graph edge, and which are not.
        //
        // A number or a date is a MEASUREMENT, not a thing to connect to — an
        // edge to the entity "5" would join every task with five points into a
        // meaningless cluster. Select, multi-select and relation name things;
        // text and url are freeform and belong to the extractor, which already
        // reads them as prose.
        let targets: Vec<String> = match kind {
            PropertyKind::Select => value.as_str().map(|s| vec![s.to_string()]).unwrap_or_default(),
            PropertyKind::MultiSelect => value
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            PropertyKind::Relation => {
                // A relation's target is a PAGE, so the edge joins two page
                // entities — which is what makes "blocked by" traversable.
                let ids: Vec<Uuid> = crate::relations::forward(pool, property_id, page_id)
                    .await
                    .unwrap_or_default();
                let mut names = Vec::new();
                for id in ids {
                    if let Some(t) = sqlx::query_scalar::<_, String>(
                        "SELECT title FROM pages WHERE id = $1 AND archived_at IS NULL",
                    )
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?
                    {
                        names.push(t);
                    }
                }
                names
            }
            _ => Vec::new(),
        };

        for target_name in targets {
            let target = crate::graph::resolve_entity_tx(
                &mut tx,
                workspace_id,
                &crate::graph::normalise_for_graph(&target_name),
                Some("VALUE"),
                None,
            )
            .await?;

            // The relation is the PROPERTY'S NAME, lowercased. "Status" becomes
            // `status`, "Blocked by" becomes `blocked_by` — so the graph reads
            // as the user's own vocabulary rather than a generic `has_value`.
            let relation = prop_name.trim().to_lowercase().replace(' ', "_");

            sqlx::query(
                "INSERT INTO property_edges
                     (workspace_id, source_entity, target_entity, relation, weight, property_id, page_id)
                 VALUES ($1, $2, $3, $4, 1.0, $5, $6)
                 ON CONFLICT DO NOTHING",
            )
            .bind(workspace_id)
            .bind(subject)
            .bind(target)
            .bind(&relation)
            .bind(property_id)
            .bind(page_id)
            .execute(&mut *tx)
            .await?;
            written += 1;
        }
    }

    tx.commit().await?;
    Ok(written)
}

/// How many property-derived edges a workspace holds — the counterpart to the
/// chunk-derived count in `stats`.
pub async fn count(pool: &sqlx::PgPool, workspace_id: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM property_edges pe
         JOIN pages p ON p.id = pe.page_id AND p.archived_at IS NULL
         WHERE pe.workspace_id = $1",
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await
}

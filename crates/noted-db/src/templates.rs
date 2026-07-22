//! Page templates: save a page's shape, stamp out new pages from it.
use std::collections::HashMap;

use serde_json::Value as Json;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, serde::Serialize, sqlx::FromRow)]
pub struct Template {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub title_pattern: String,
    pub blocks: Json,
    pub properties: Json,
}

/// Substitute `{{name}}` placeholders.
///
/// An UNKNOWN placeholder is left EXACTLY as it was, rather than replaced with
/// an empty string. A template that silently emptied `{{cleint}}` would produce
/// a page with a hole in it that reads as finished; leaving the marker visible
/// makes the typo obvious at the moment someone reads the new page.
pub fn substitute(pattern: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut rest = pattern;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let key = after[..end].trim();
                match vars.get(key) {
                    Some(value) => out.push_str(value),
                    // Unknown: put the marker back, verbatim.
                    None => {
                        out.push_str("{{");
                        out.push_str(&after[..end]);
                        out.push_str("}}");
                    }
                }
                rest = &after[end + 2..];
            }
            // An unclosed `{{` is literal text, not the start of a variable.
            None => {
                out.push_str("{{");
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Save a page's current shape as a template.
pub async fn save_from_page(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    page_id: Uuid,
    name: &str,
) -> Result<Template, sqlx::Error> {
    let title: String = sqlx::query_scalar("SELECT title FROM pages WHERE id = $1")
        .bind(page_id)
        .fetch_one(pool)
        .await?;

    let blocks: Vec<(String, String)> = sqlx::query_as(
        "SELECT node_type, text FROM blocks WHERE page_id = $1 ORDER BY block_index",
    )
    .bind(page_id)
    .fetch_all(pool)
    .await?;
    let blocks_json = Json::Array(
        blocks
            .into_iter()
            .map(|(node_type, text)| serde_json::json!({"node_type": node_type, "text": text}))
            .collect(),
    );

    let props: Vec<(Uuid, Json)> =
        sqlx::query_as("SELECT property_id, value FROM page_properties WHERE page_id = $1")
            .bind(page_id)
            .fetch_all(pool)
            .await?;
    let props_json = Json::Object(
        props
            .into_iter()
            .map(|(id, v)| (id.to_string(), v))
            .collect(),
    );

    sqlx::query_as::<_, Template>(
        "INSERT INTO templates (workspace_id, name, title_pattern, blocks, properties)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id, workspace_id, name, title_pattern, blocks, properties",
    )
    .bind(workspace_id)
    .bind(name)
    .bind(&title)
    .bind(blocks_json)
    .bind(props_json)
    .fetch_one(pool)
    .await
}

pub async fn get(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<Template>, sqlx::Error> {
    sqlx::query_as::<_, Template>(
        "SELECT id, workspace_id, name, title_pattern, blocks, properties
         FROM templates WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Create a page from a template.
///
/// # The blocks are written to `blocks`, which is what makes the page INDEX
///
/// A template that only set a title would produce a page the pipeline sees as
/// empty: no blocks means no chunks, which means no embeddings, no graph, and
/// nothing findable. Writing the blocks here is the whole reason
/// `an_instantiated_page_is_indexed_like_any_other` exists.
///
/// The projection normally comes from the CRDT document, and it still will the
/// moment someone edits the page. Seeding `blocks` directly makes the page
/// searchable BEFORE anyone opens it, which is what a template is for.
pub async fn instantiate(
    pool: &sqlx::PgPool,
    template_id: Uuid,
    parent_id: Option<Uuid>,
    vars: &HashMap<String, String>,
) -> Result<Uuid, sqlx::Error> {
    let t = get(pool, template_id).await?.ok_or(sqlx::Error::RowNotFound)?;

    let title = substitute(&t.title_pattern, vars);
    let mut tx = pool.begin().await?;

    let page_id: Uuid = sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, parent_id, title) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(t.workspace_id)
    .bind(parent_id)
    .bind(&title)
    .fetch_one(&mut *tx)
    .await?;

    for (i, block) in t.blocks.as_array().unwrap_or(&Vec::new()).iter().enumerate() {
        let node_type = block
            .get("node_type")
            .and_then(Json::as_str)
            .unwrap_or("paragraph");
        let text = substitute(block.get("text").and_then(Json::as_str).unwrap_or(""), vars);
        sqlx::query(
            "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
             VALUES ($1, $2, $3, $4, md5($3 || $4))",
        )
        .bind(page_id)
        .bind(i as i32)
        .bind(node_type)
        .bind(&text)
        .execute(&mut *tx)
        .await?;
    }

    for (prop, value) in t.properties.as_object().unwrap_or(&serde_json::Map::new()) {
        let Ok(property_id) = Uuid::parse_str(prop) else {
            continue;
        };
        // Substituted too: a template can carry `{{owner}}` in a select as
        // easily as in a paragraph.
        let value = match value.as_str() {
            Some(s) => Json::from(substitute(s, vars)),
            None => value.clone(),
        };
        sqlx::query(
            "INSERT INTO page_properties (page_id, property_id, value)
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(page_id)
        .bind(property_id)
        .bind(value)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(page_id)
}

pub async fn for_workspace(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
) -> Result<Vec<Template>, sqlx::Error> {
    sqlx::query_as::<_, Template>(
        "SELECT id, workspace_id, name, title_pattern, blocks, properties
         FROM templates WHERE workspace_id = $1 ORDER BY created_at, name",
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await
}

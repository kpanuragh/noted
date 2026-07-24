//! M5-3 — page templates.
use std::collections::HashMap;

use noted_db::collections::{self, PropertyKind};
use noted_db::templates;
use serde_json::json;
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let p = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&p).await.unwrap();
    p
}
async fn ws(p: &noted_db::PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('tmpl') RETURNING id")
        .fetch_one(p).await.unwrap()
}
async fn page_with_blocks(p: &noted_db::PgPool, w: Uuid, title: &str, texts: &[&str]) -> Uuid {
    let id: Uuid = sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1,$2) RETURNING id")
        .bind(w).bind(title).fetch_one(p).await.unwrap();
    for (i, t) in texts.iter().enumerate() {
        sqlx::query("INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
                     VALUES ($1,$2,'paragraph',$3, md5($3))")
            .bind(id).bind(i as i32).bind(t).execute(p).await.unwrap();
    }
    id
}
fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[test]
fn substitution_fills_known_variables() {
    let v = vars(&[("client", "Acme"), ("date", "2026-07-22")]);
    assert_eq!(templates::substitute("{{client}} kickoff", &v), "Acme kickoff");
    assert_eq!(templates::substitute("{{ client }} on {{date}}", &v), "Acme on 2026-07-22");
    assert_eq!(templates::substitute("no variables here", &v), "no variables here");
}

/// **An unknown variable is left VISIBLE, not silently emptied.**
///
/// A template that quietly turned `{{cleint}}` into nothing would produce a
/// page with a hole in it that reads as finished. Leaving the marker makes the
/// typo obvious the moment someone reads the page.
#[test]
fn an_unknown_variable_is_left_in_place_rather_than_emptied() {
    let v = vars(&[("client", "Acme")]);
    assert_eq!(
        templates::substitute("{{cleint}} kickoff for {{client}}", &v),
        "{{cleint}} kickoff for Acme"
    );
}

/// Malformed markers are literal text, not a parse error and not a swallowed
/// remainder.
#[test]
fn an_unclosed_marker_is_literal_text() {
    let v = vars(&[("a", "1")]);
    assert_eq!(templates::substitute("{{a}} and {{unclosed", &v), "1 and {{unclosed");
    assert_eq!(templates::substitute("100%{ not a marker", &v), "100%{ not a marker");
}

/// A template captures the page's blocks, and instantiating reproduces them.
#[tokio::test]
async fn a_template_round_trips_a_pages_blocks() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let src = page_with_blocks(&pool, w, "{{client}} kickoff",
        &["Agenda for {{client}}", "Owner: {{owner}}"]).await;

    let t = templates::save_from_page(&pool, w, src, "Kickoff").await.unwrap();
    let new = templates::instantiate(&pool, t.id, None,
        &vars(&[("client", "Acme"), ("owner", "Alice")])).await.unwrap();

    let title: String = sqlx::query_scalar("SELECT title FROM pages WHERE id = $1")
        .bind(new).fetch_one(&pool).await.unwrap();
    assert_eq!(title, "Acme kickoff");

    let texts: Vec<String> = sqlx::query_scalar(
        "SELECT text FROM blocks WHERE page_id = $1 ORDER BY block_index")
        .bind(new).fetch_all(&pool).await.unwrap();
    assert_eq!(texts, vec!["Agenda for Acme", "Owner: Alice"]);
}

/// **An instantiated page is indexed like any other.**
///
/// This is the acceptance criterion the issue names, and the reason
/// `instantiate` writes `blocks` rather than only a title: no blocks means no
/// chunks, which means no embeddings, no graph, and a page nothing can find.
///
/// MECHANISM PROTECTED: the block INSERT loop. Remove it and the page exists
/// but is invisible to every retrieval surface.
#[tokio::test]
async fn an_instantiated_page_is_indexed_like_any_other() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let src = page_with_blocks(&pool, w, "Retro", &["What went well", "What did not"]).await;
    let t = templates::save_from_page(&pool, w, src, "Retro").await.unwrap();
    let new = templates::instantiate(&pool, t.id, None, &HashMap::new()).await.unwrap();

    // Blocks exist — the projection the chunker reads.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM blocks WHERE page_id = $1")
        .bind(new).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 2, "no blocks would mean no chunks and nothing findable");

    // And full-text search can see the text, which is the real test of
    // "indexed like any other".
    let found: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM blocks b JOIN pages p ON p.id = b.page_id
         WHERE p.workspace_id = $1
           AND to_tsvector('english', b.text) @@ plainto_tsquery('english', 'went well')")
        .bind(w).fetch_one(&pool).await.unwrap();
    assert!(found >= 1, "the instantiated page's text must be searchable");
}

/// A template carries property values, substituted like any other text.
#[tokio::test]
async fn a_template_carries_property_values() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let host = page_with_blocks(&pool, w, "Tasks", &[]).await;
    let c = collections::create_collection(&pool, w, host, "Tasks").await.unwrap();
    let owner = collections::add_property(&pool, c.id, "Owner", PropertyKind::Text, json!({}), 0)
        .await.unwrap().id;

    let src = page_with_blocks(&pool, w, "Task", &["body"]).await;
    collections::set_value(&pool, src, owner, json!("{{owner}}")).await.unwrap();

    let t = templates::save_from_page(&pool, w, src, "Task").await.unwrap();
    let new = templates::instantiate(&pool, t.id, Some(host), &vars(&[("owner", "Bob")])).await.unwrap();

    let v: serde_json::Value = sqlx::query_scalar(
        "SELECT value FROM page_properties WHERE page_id = $1 AND property_id = $2")
        .bind(new).bind(owner).fetch_one(&pool).await.unwrap();
    assert_eq!(v, json!("Bob"), "a variable in a property is substituted too");
}

/// **A template is not a page**, so it never appears in the tree or in search.
///
/// Storing templates as pages with a flag would need a new predicate in every
/// query that lists pages, and the one that forgot it would leak a template
/// into someone's notes.
#[tokio::test]
async fn a_template_never_appears_as_a_page() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let src = page_with_blocks(&pool, w, "Source", &["text"]).await;
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM pages WHERE workspace_id = $1")
        .bind(w).fetch_one(&pool).await.unwrap();

    templates::save_from_page(&pool, w, src, "Tmpl").await.unwrap();

    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM pages WHERE workspace_id = $1")
        .bind(w).fetch_one(&pool).await.unwrap();
    assert_eq!(before, after, "saving a template must not create a page");
    assert_eq!(templates::for_workspace(&pool, w).await.unwrap().len(), 1);
}

/// Instantiating twice makes two independent pages — editing one must not
/// change the other, and neither must change the template.
#[tokio::test]
async fn each_instantiation_is_independent() {
    let pool = pool().await;
    let w = ws(&pool).await;
    let src = page_with_blocks(&pool, w, "T", &["shared text"]).await;
    let t = templates::save_from_page(&pool, w, src, "T").await.unwrap();

    let a = templates::instantiate(&pool, t.id, None, &HashMap::new()).await.unwrap();
    let b = templates::instantiate(&pool, t.id, None, &HashMap::new()).await.unwrap();
    assert_ne!(a, b);

    sqlx::query("UPDATE blocks SET text = 'edited' WHERE page_id = $1").bind(a)
        .execute(&pool).await.unwrap();

    let b_text: String = sqlx::query_scalar("SELECT text FROM blocks WHERE page_id = $1")
        .bind(b).fetch_one(&pool).await.unwrap();
    assert_eq!(b_text, "shared text", "instances must not share storage");

    let t_after = templates::get(&pool, t.id).await.unwrap().unwrap();
    assert_eq!(t_after.blocks[0]["text"], json!("shared text"), "nor edit the template");
}

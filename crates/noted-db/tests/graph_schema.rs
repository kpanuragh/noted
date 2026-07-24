async fn pool() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let p = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&p).await.unwrap();
    p
}

#[tokio::test]
async fn graph_tables_exist() {
    let p = pool().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables
         WHERE table_name IN ('entities','edges','chunk_extractions')")
        .fetch_one(&p).await.unwrap();
    assert_eq!(n, 3);
}

#[tokio::test]
async fn entity_name_is_unique_per_workspace_not_globally() {
    let p = pool().await;
    let a: uuid::Uuid = sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('a') RETURNING id").fetch_one(&p).await.unwrap();
    let b: uuid::Uuid = sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('b') RETURNING id").fetch_one(&p).await.unwrap();
    // Same name in two workspaces: allowed (different graph nodes).
    sqlx::query("INSERT INTO entities (workspace_id, name, entity_type) VALUES ($1,'postgres','CONCEPT')").bind(a).execute(&p).await.unwrap();
    sqlx::query("INSERT INTO entities (workspace_id, name, entity_type) VALUES ($1,'postgres','CONCEPT')").bind(b).execute(&p).await.unwrap();
    // Same name twice in ONE workspace: rejected (resolution key).
    let dup = sqlx::query("INSERT INTO entities (workspace_id, name, entity_type) VALUES ($1,'postgres','CONCEPT')").bind(a).execute(&p).await;
    assert!(dup.is_err(), "an entity name must be unique within a workspace");
}

#[tokio::test]
async fn an_edge_requires_its_source_chunk_to_exist() {
    let p = pool().await;
    let ws: uuid::Uuid = sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('e') RETURNING id").fetch_one(&p).await.unwrap();
    let e1: uuid::Uuid = sqlx::query_scalar("INSERT INTO entities (workspace_id, name, entity_type) VALUES ($1,'x','C') RETURNING id").bind(ws).fetch_one(&p).await.unwrap();
    let e2: uuid::Uuid = sqlx::query_scalar("INSERT INTO entities (workspace_id, name, entity_type) VALUES ($1,'y','C') RETURNING id").bind(ws).fetch_one(&p).await.unwrap();
    let orphan = sqlx::query(
        "INSERT INTO edges (source_entity,target_entity,relation,source_chunk_hash,model_id,workspace_id)
         VALUES ($1,$2,'rel','no-such-chunk','m',$3)").bind(e1).bind(e2).bind(ws).execute(&p).await;
    assert!(orphan.is_err(), "an edge must FK to a real chunk (provenance)");
}

#[tokio::test]
async fn deleting_a_chunk_cascades_to_its_edges() {
    let p = pool().await;
    let ws: uuid::Uuid = sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('c') RETURNING id").fetch_one(&p).await.unwrap();
    let e1: uuid::Uuid = sqlx::query_scalar("INSERT INTO entities (workspace_id, name, entity_type) VALUES ($1,'x','C') RETURNING id").bind(ws).fetch_one(&p).await.unwrap();
    let e2: uuid::Uuid = sqlx::query_scalar("INSERT INTO entities (workspace_id, name, entity_type) VALUES ($1,'y','C') RETURNING id").bind(ws).fetch_one(&p).await.unwrap();
    let h = format!("gh-{}", uuid::Uuid::new_v4());
    sqlx::query("INSERT INTO chunks (content_hash, text, token_estimate) VALUES ($1,'t',1)").bind(&h).execute(&p).await.unwrap();
    sqlx::query("INSERT INTO edges (source_entity,target_entity,relation,source_chunk_hash,model_id,workspace_id) VALUES ($1,$2,'r',$3,'m',$4)").bind(e1).bind(e2).bind(&h).bind(ws).execute(&p).await.unwrap();
    sqlx::query("DELETE FROM chunks WHERE content_hash=$1").bind(&h).execute(&p).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM edges WHERE source_chunk_hash=$1").bind(&h).fetch_one(&p).await.unwrap();
    assert_eq!(n, 0, "deleting a chunk must cascade to edges it sourced");
}

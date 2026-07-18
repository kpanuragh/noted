//! MIGRATION SEMANTICS — the one thing the rest of the suite cannot cover.
//!
//! Every other test in this crate runs against a database that is ALREADY
//! fully migrated, so it exercises the post-0008 world through the repository
//! functions. That is exactly why the original 0008 backfill bug survived
//! review: `graph.rs::a_workspace_that_joins_a_shared_chunk_after_extraction_
//! still_gets_a_graph` reaches the same scenario through the WORKER, on a
//! database where 0008's backfill had nothing to do, so it stayed green while
//! the backfill itself re-created the very bug 0008 exists to fix.
//!
//! These tests therefore run the migration's REAL SQL text (`include_str!`, so
//! editing the migration edits the test's subject) against a hand-built
//! 0005-era table layout, inside a throwaway schema in a transaction that is
//! always rolled back. Nothing is left behind and the shared dev database is
//! untouched.
use sqlx::Connection;

const MIGRATION_0008: &str = include_str!("../migrations/0008_chunk_extractions_workspace.sql");

/// The 0005-era world 0008 has to upgrade: a GLOBAL `chunk_extractions` keyed
/// `(content_hash, model_id)`, plus just enough of `workspaces`/`pages`/
/// `chunks`/`page_chunks`/`entities`/`edges` for 0008's FKs and backfill query
/// to resolve. Deliberately minimal — this is the shape 0008 reads, not a copy
/// of the whole schema.
const ERA_0005: &str = r#"
CREATE TABLE workspaces (
    id   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name text NOT NULL
);
CREATE TABLE pages (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    archived_at  timestamptz
);
CREATE TABLE chunks (
    content_hash text PRIMARY KEY,
    text         text NOT NULL
);
CREATE TABLE page_chunks (
    page_id      uuid NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    chunk_index  int  NOT NULL,
    content_hash text NOT NULL REFERENCES chunks(content_hash),
    PRIMARY KEY (page_id, chunk_index)
);
CREATE TABLE entities (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name         text NOT NULL
);
CREATE TABLE edges (
    source_entity     uuid NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    target_entity     uuid NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    relation          text NOT NULL,
    weight            real NOT NULL DEFAULT 1.0,
    source_chunk_hash text NOT NULL REFERENCES chunks(content_hash) ON DELETE CASCADE,
    model_id          text NOT NULL,
    workspace_id      uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    PRIMARY KEY (source_entity, target_entity, relation, source_chunk_hash, model_id)
);
CREATE TABLE chunk_extractions (
    content_hash text NOT NULL REFERENCES chunks(content_hash) ON DELETE CASCADE,
    model_id     text NOT NULL,
    extracted_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (content_hash, model_id)
);
"#;

async fn conn() -> sqlx::PgConnection {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    sqlx::PgConnection::connect(&url).await.unwrap()
}

/// Build a throwaway schema holding the 0005-era layout and point the
/// transaction's `search_path` at it, so the migration's unqualified table
/// names (and its FK targets) resolve there and nothing touches the real
/// schema. The caller always rolls back, so even the schema itself never
/// lands — DDL is transactional in Postgres.
async fn era_0005(tx: &mut sqlx::PgTransaction<'_>) {
    let schema = format!("mig0008_{}", uuid::Uuid::new_v4().simple());
    // SAFETY (AssertSqlSafe): `schema` is not user input — it is a fixed
    // prefix plus a locally generated UUID's hex simple form, so it can only
    // ever be `[a-z0-9_]`. Postgres has no bind-parameter form for a schema
    // name in CREATE SCHEMA / SET search_path, so interpolation is the only
    // option here. Same precedent as `pages.rs`'s interpolated column const.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema};"
    )))
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::raw_sql(ERA_0005).execute(&mut **tx).await.unwrap();
}

/// THE BUG THIS TEST EXISTS FOR.
///
/// 0008's header describes the population it is written to rescue: a workspace
/// that adopts an already-extracted SHARED chunk LATER, after the global
/// marker was written. Such a workspace has no edges for that chunk and was
/// never queued.
///
/// The original backfill fanned every global marker onto EVERY workspace
/// referencing the chunk, justified by "the worker writes every referencing
/// workspace's edges before marking, so all N were in fact written". That
/// holds only for workspaces referencing the chunk AT THE MOMENT OF
/// EXTRACTION. For the late joiner — precisely the case above — it stamps
/// "extracted" on a workspace that has no graph at all, permanently and
/// silently: the new per-workspace queue then skips it forever and
/// `extraction_progress` reports it complete.
///
/// The fix backfills only where edges for that `(workspace_id, content_hash,
/// model_id)` actually exist.
#[tokio::test]
async fn the_0008_backfill_does_not_mark_a_late_joining_workspace_that_has_no_edges() {
    let mut c = conn().await;
    let mut tx = c.begin().await.unwrap();
    era_0005(&mut tx).await;

    // Workspace A extracted the chunk: it has edges AND the (then global)
    // marker. Workspace B adopted the same content-addressed chunk afterwards:
    // a live page referencing it, and NO edges — it was never queued, because
    // the global marker already said "done".
    sqlx::raw_sql(
        "INSERT INTO workspaces (id, name) VALUES
             ('00000000-0000-0000-0000-0000000000aa', 'early'),
             ('00000000-0000-0000-0000-0000000000bb', 'late');
         INSERT INTO pages (id, workspace_id) VALUES
             ('00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-0000000000aa'),
             ('00000000-0000-0000-0000-0000000000b1', '00000000-0000-0000-0000-0000000000bb');
         INSERT INTO chunks (content_hash, text) VALUES ('h-shared', 'identical text');
         INSERT INTO page_chunks (page_id, chunk_index, content_hash) VALUES
             ('00000000-0000-0000-0000-0000000000a1', 0, 'h-shared'),
             ('00000000-0000-0000-0000-0000000000b1', 0, 'h-shared');
         INSERT INTO entities (id, workspace_id, name) VALUES
             ('00000000-0000-0000-0000-0000000000e1', '00000000-0000-0000-0000-0000000000aa', 'x'),
             ('00000000-0000-0000-0000-0000000000e2', '00000000-0000-0000-0000-0000000000aa', 'y');
         INSERT INTO edges (source_entity, target_entity, relation, source_chunk_hash, model_id, workspace_id)
         VALUES ('00000000-0000-0000-0000-0000000000e1',
                 '00000000-0000-0000-0000-0000000000e2',
                 'mentions_with', 'h-shared', 'm', '00000000-0000-0000-0000-0000000000aa');
         INSERT INTO chunk_extractions (content_hash, model_id) VALUES ('h-shared', 'm');",
    )
    .execute(&mut *tx)
    .await
    .unwrap();

    sqlx::raw_sql(MIGRATION_0008)
        .execute(&mut *tx)
        .await
        .unwrap();

    let marked: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT workspace_id FROM chunk_extractions
         WHERE content_hash = 'h-shared' AND model_id = 'm'
         ORDER BY workspace_id",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();

    assert_eq!(
        marked,
        vec![uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000aa").unwrap()],
        "only the workspace that actually has edges for this chunk may be carried over as \
         extracted. Marking the late joiner is permanent and silent: it would never be queued \
         again and progress would report it complete with an empty graph."
    );

    tx.rollback().await.unwrap();
}

/// The other half of the trade-off, asserted so it cannot regress by accident:
/// under-marking is fine, over-marking is not. A workspace WITH edges keeps
/// its marker (no needless re-extraction), and a marker whose chunk no live
/// page references at all is dropped as unattributable.
#[tokio::test]
async fn the_0008_backfill_keeps_markers_that_have_edges_and_drops_unattributable_ones() {
    let mut c = conn().await;
    let mut tx = c.begin().await.unwrap();
    era_0005(&mut tx).await;

    sqlx::raw_sql(
        "INSERT INTO workspaces (id, name) VALUES
             ('00000000-0000-0000-0000-0000000000aa', 'w');
         INSERT INTO pages (id, workspace_id) VALUES
             ('00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-0000000000aa');
         INSERT INTO chunks (content_hash, text) VALUES ('h-live', 't'), ('h-orphan', 't');
         INSERT INTO page_chunks (page_id, chunk_index, content_hash) VALUES
             ('00000000-0000-0000-0000-0000000000a1', 0, 'h-live');
         INSERT INTO entities (id, workspace_id, name) VALUES
             ('00000000-0000-0000-0000-0000000000e1', '00000000-0000-0000-0000-0000000000aa', 'x'),
             ('00000000-0000-0000-0000-0000000000e2', '00000000-0000-0000-0000-0000000000aa', 'y');
         INSERT INTO edges (source_entity, target_entity, relation, source_chunk_hash, model_id, workspace_id)
         VALUES ('00000000-0000-0000-0000-0000000000e1',
                 '00000000-0000-0000-0000-0000000000e2',
                 'mentions_with', 'h-live', 'm', '00000000-0000-0000-0000-0000000000aa');
         INSERT INTO chunk_extractions (content_hash, model_id) VALUES ('h-live', 'm'), ('h-orphan', 'm');",
    )
    .execute(&mut *tx)
    .await
    .unwrap();

    sqlx::raw_sql(MIGRATION_0008)
        .execute(&mut *tx)
        .await
        .unwrap();

    let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT workspace_id, content_hash FROM chunk_extractions ORDER BY content_hash",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();

    assert_eq!(
        rows,
        vec![(
            uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000aa").unwrap(),
            "h-live".to_string()
        )],
        "a marker backed by real edges must survive (re-extracting it would be pure waste); an \
         orphaned chunk's marker has no workspace to attribute it to and is dropped"
    );

    tx.rollback().await.unwrap();
}

/// M4: the table is REBUILT as `chunk_extractions_new` and renamed, but
/// `ALTER TABLE ... RENAME` does not rename dependent constraints or indexes.
/// Without an explicit rename the primary key is stuck as
/// `chunk_extractions_new_pkey` forever, so any future migration that names
/// `chunk_extractions_pkey` (the name every other table in this schema has)
/// fails.
#[tokio::test]
async fn migration_0008_leaves_constraints_with_their_proper_names() {
    let mut c = conn().await;
    let mut tx = c.begin().await.unwrap();
    era_0005(&mut tx).await;
    sqlx::raw_sql(MIGRATION_0008)
        .execute(&mut *tx)
        .await
        .unwrap();

    let names: Vec<String> = sqlx::query_scalar(
        "SELECT conname FROM pg_constraint
         WHERE conrelid = 'chunk_extractions'::regclass
         ORDER BY conname",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();

    assert_eq!(
        names,
        vec![
            "chunk_extractions_content_hash_fkey".to_string(),
            "chunk_extractions_pkey".to_string(),
            "chunk_extractions_workspace_id_fkey".to_string(),
        ],
        "constraints must not keep the `_new` scaffolding names of the rebuild"
    );

    tx.rollback().await.unwrap();
}

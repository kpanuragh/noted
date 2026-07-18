//! Graph repository: entity resolution, provenance edge writes, and the
//! extraction work queue. This module deals ONLY in primitives (`Uuid`,
//! `String`, tuples) — never the `Extraction` type, which lives in
//! `noted-index`. `noted-index` already depends on `noted-db`, so `noted-db`
//! must never depend back on `noted-index`; the `Extraction` -> resolved-rows
//! mapping is `noted-index`'s job, not this module's.
use uuid::Uuid;

/// Resolve an entity name to its id, creating the entity if it doesn't exist
/// yet.
///
/// `name` MUST already be normalised by the caller — normalisation
/// (whitespace-collapsing, lowercasing) lives in
/// `noted_index::extract::normalise_entity`. This function does not
/// normalise; it treats `name` as the literal resolution key, scoped to
/// `(workspace_id, name)`.
///
/// If the entity already exists, its `description` is kept unless the new
/// call supplies one (`COALESCE(EXCLUDED.description, entities.description)`)
/// — a later extraction pass with no description must not blank out one an
/// earlier pass wrote.
pub async fn resolve_entity(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    name: &str,
    entity_type: &str,
    description: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO entities (workspace_id, name, entity_type, description)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (workspace_id, name) DO UPDATE
           SET description = COALESCE(EXCLUDED.description, entities.description)
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(name)
    .bind(entity_type)
    .bind(description)
    .fetch_one(pool)
    .await
}

/// Replace one chunk's extracted edges for one model, IN ONE WORKSPACE.
///
/// Does NOT write the `chunk_extractions` marker — see `mark_extracted`'s
/// doc comment for why that is now a separate call. Runs in ONE transaction:
/// DELETE the chunk's existing edges for this model **within
/// `workspace_id`**, then insert the new set. A crash mid-write rolls the
/// whole thing back, so this workspace's edges for this chunk are always
/// either fully replaced or untouched — never half.
///
/// The DELETE is scoped by `workspace_id` as well as `(source_chunk_hash,
/// model_id)`. This matters because `source_chunk_hash` is a GLOBAL,
/// content-addressed key (chunks are shared across workspaces when their
/// text is byte-identical — M1b) but edges belong to a single workspace (they
/// connect two entities that are themselves per-workspace). Without the
/// workspace scope, workspace B extracting a chunk workspace A already
/// extracted would delete A's edges for that chunk out from under it — see
/// migration `0007_edges_workspace.sql` and
/// `noted-index/tests/incremental.rs::incremental_extraction_equals_a_full_rebuild`,
/// which caught exactly this.
///
/// `edges` tuples are `(source_entity_id, target_entity_id, relation, weight)`.
/// Insert uses `ON CONFLICT ... DO UPDATE SET weight = EXCLUDED.weight`
/// rather than a bare insert: the `edges` primary key is
/// `(source_entity, target_entity, relation, source_chunk_hash, model_id)`,
/// which excludes `weight` — so the same chunk asserting the same relation
/// between the same pair at two different weights (or the stub emitting the
/// same edge twice within one extraction) would otherwise PK-violate. The
/// last-written weight wins.
///
/// Postgres additionally refuses to let one `INSERT ... ON CONFLICT DO
/// UPDATE` affect the same row twice ("ON CONFLICT DO UPDATE command cannot
/// affect row a second time") — so a duplicate edge *within the same slice*
/// would still crash the multi-row insert even with the clause above. We
/// dedupe by `(source, target, relation)` before building it, keeping the
/// last occurrence — same last-write-wins semantics as the ON CONFLICT
/// clause itself, just applied client-side first.
pub async fn replace_chunk_edges(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    chunk_hash: &str,
    model_id: &str,
    edges: &[(Uuid, Uuid, String, f32)],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    sqlx::query(
        "DELETE FROM edges WHERE workspace_id = $1 AND source_chunk_hash = $2 AND model_id = $3",
    )
    .bind(workspace_id)
    .bind(chunk_hash)
    .bind(model_id)
    .execute(&mut *tx)
    .await?;

    if !edges.is_empty() {
        let mut deduped: Vec<(Uuid, Uuid, String, f32)> = Vec::with_capacity(edges.len());
        for edge in edges {
            if let Some(existing) = deduped
                .iter_mut()
                .find(|e| e.0 == edge.0 && e.1 == edge.1 && e.2 == edge.2)
            {
                existing.3 = edge.3;
            } else {
                deduped.push(edge.clone());
            }
        }

        let sources: Vec<Uuid> = deduped.iter().map(|e| e.0).collect();
        let targets: Vec<Uuid> = deduped.iter().map(|e| e.1).collect();
        let relations: Vec<String> = deduped.iter().map(|e| e.2.clone()).collect();
        let weights: Vec<f32> = deduped.iter().map(|e| e.3).collect();

        sqlx::query(
            "INSERT INTO edges (source_entity, target_entity, relation, weight, source_chunk_hash, model_id, workspace_id)
             SELECT s, t, r, w, $5, $6, $7
             FROM UNNEST($1::uuid[], $2::uuid[], $3::text[], $4::real[]) AS x(s, t, r, w)
             ON CONFLICT (source_entity, target_entity, relation, source_chunk_hash, model_id)
             DO UPDATE SET weight = EXCLUDED.weight",
        )
        .bind(&sources)
        .bind(&targets)
        .bind(&relations)
        .bind(&weights)
        .bind(chunk_hash)
        .bind(model_id)
        .bind(workspace_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await
}

/// Mark a chunk extracted for `model_id` — the thing that removes it from
/// `pending_extraction`.
///
/// Split out of `replace_chunk_edges` (which used to write this marker
/// itself, transactionally, right after the edge insert) because of the
/// workspace subtlety: `content_hash` is a GLOBAL, content-addressed key
/// (M1b) that can be referenced by live pages in MULTIPLE workspaces, but
/// `chunk_extractions` has no `workspace_id` column — the marker is
/// necessarily all-or-nothing per chunk, not per workspace. If
/// `replace_chunk_edges` still set it, the FIRST workspace to extract a
/// shared chunk would remove it from the queue before the other workspaces
/// referencing it ever got their own graph written, and a crash between
/// workspaces would strand them unextracted forever with no way back into
/// the queue.
///
/// The correct order (enforced by the caller, `noted-index`'s
/// `ExtractWorker`): extract the chunk's text ONCE, call
/// `replace_chunk_edges` once per workspace that references it, and only
/// after ALL of those succeed, call `mark_extracted` ONCE. `ON CONFLICT DO
/// NOTHING` keeps a second call (e.g. a retry after the marker already
/// landed) a no-op rather than an error.
pub async fn mark_extracted(
    pool: &sqlx::PgPool,
    content_hash: &str,
    model_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO chunk_extractions (content_hash, model_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(content_hash)
    .bind(model_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Distinct workspaces whose LIVE pages currently reference `content_hash`.
///
/// A chunk is content-addressed and globally shared (M1b): two workspaces
/// can each have a live page containing byte-identical text, and each such
/// workspace needs its OWN copy of the extraction (entities/edges are scoped
/// per-workspace — see `resolve_entity`'s docs). The extraction worker calls
/// this once per pending chunk to fan its single `extract()` call out to
/// every workspace that needs the result written.
pub async fn workspaces_for_chunk(
    pool: &sqlx::PgPool,
    content_hash: &str,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT DISTINCT p.workspace_id
         FROM page_chunks pc
         JOIN pages p ON p.id = pc.page_id
         WHERE pc.content_hash = $1",
    )
    .bind(content_hash)
    .fetch_all(pool)
    .await
}

/// THE EXTRACTION WORK QUEUE. Not a table — a set difference, mirroring
/// `chunks::pending`. Every chunk referenced by a live page (via
/// `page_chunks`, NOT `blocks` — see `chunks::pending`'s note on why those
/// hash spaces never join) that has no `chunk_extractions` row for
/// `model_id` yet.
///
/// `workspace_id: None` drains the whole instance — what the CLI wants, and
/// what `chunks::pending` (its embedding sibling) does unconditionally.
/// `Some(id)` scopes the queue to chunks referenced by a live page in that
/// one workspace — required so a per-tenant extraction run (or a test on a
/// shared dev database) does not pull in every OTHER workspace's pending
/// chunks too. Mirrors `extraction_progress`'s `$2::uuid IS NULL OR
/// p.workspace_id = $2` scoping exactly, joining through `pages` the same
/// way. Keeps the query string `'static` (bind, never interpolate) and the
/// existing `ORDER BY`/set-difference shape untouched.
pub async fn pending_extraction(
    pool: &sqlx::PgPool,
    model_id: &str,
    workspace_id: Option<Uuid>,
    limit: i64,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT DISTINCT c.content_hash, c.text
         FROM page_chunks pc
         JOIN pages p ON p.id = pc.page_id
         JOIN chunks c ON c.content_hash = pc.content_hash
         LEFT JOIN chunk_extractions ce
           ON ce.content_hash = c.content_hash AND ce.model_id = $1
         WHERE ce.content_hash IS NULL
           AND ($3::uuid IS NULL OR p.workspace_id = $3)
         ORDER BY c.content_hash
         LIMIT $2",
    )
    .bind(model_id)
    .bind(limit)
    .bind(workspace_id)
    .fetch_all(pool)
    .await
}

/// (extracted, total) over LIVE chunks under `model_id`. Mirrors
/// `chunks::progress`'s signature and semantics exactly — see its docs for
/// why `workspace_id: None` vs `Some` matters and why counting through
/// `page_chunks` keeps orphaned chunks from dragging the denominator down.
pub async fn extraction_progress(
    pool: &sqlx::PgPool,
    model_id: &str,
    workspace_id: Option<Uuid>,
) -> Result<(i64, i64), sqlx::Error> {
    sqlx::query_as(
        "SELECT
           count(*) FILTER (WHERE ce.content_hash IS NOT NULL) AS extracted,
           count(*)                                             AS total
         FROM (
             SELECT DISTINCT pc.content_hash
             FROM page_chunks pc
             JOIN pages p ON p.id = pc.page_id
             WHERE $2::uuid IS NULL OR p.workspace_id = $2
         ) pc
         LEFT JOIN chunk_extractions ce
           ON ce.content_hash = pc.content_hash AND ce.model_id = $1",
    )
    .bind(model_id)
    .bind(workspace_id)
    .fetch_one(pool)
    .await
}

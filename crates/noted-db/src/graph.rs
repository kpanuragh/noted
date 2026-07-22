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
/// # Conflict semantics (a DECISION, not an accident)
///
/// `description` — WIDEN ONLY. Kept unless the new call supplies one
/// (`COALESCE(EXCLUDED.description, entities.description)`): a later
/// extraction pass with no description must not blank out one an earlier pass
/// wrote.
///
/// `entity_type` — LAST EXPLICIT WRITE WINS, which is why this parameter is
/// an `Option`, not a bare `&str`:
///
///   * `Some(t)` is an EXPLICIT classification, made by a pass that actually
///     looked at the entity. It OVERWRITES. Extraction genuinely reclassifies
///     — an entity first seen as a bare noun ("acme") and later understood as
///     an organisation should become `ORG`. Dropping that (the old behaviour,
///     which COALESCEd only `description` and silently ignored `entity_type`
///     on conflict) froze every entity at whatever its first, least-informed
///     mention guessed, with no way to ever correct it short of a manual
///     UPDATE.
///
///   * `None` means "this node exists, I do not know what it is" — how
///     `noted_index::graph_write::apply_extraction` resolves an entity that
///     appeared only as an EDGE ENDPOINT, with no `ExtractedEntity` describing
///     it. It must NOT overwrite: an unknown type is not evidence. Collapsing
///     this case into `Some("CONCEPT")` and calling it last-write-wins would
///     mean every passing mention of a known `PERSON` downgraded it back to
///     the placeholder — reclassification working in exactly the wrong
///     direction. On INSERT (nothing to preserve) `None` falls back to
///     `CONCEPT`, the same default a bare mention has always received.
///
/// Note the `DO UPDATE` clause reads `$3` directly rather than
/// `EXCLUDED.entity_type`: `EXCLUDED` holds the already-COALESCEd insert
/// value, so it could never be `NULL` and the "unknown" case would be
/// indistinguishable from an explicit `CONCEPT`.
pub async fn resolve_entity(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    name: &str,
    entity_type: Option<&str>,
    description: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO entities (workspace_id, name, entity_type, description)
         VALUES ($1, $2, COALESCE($3, 'CONCEPT'), $4)
         ON CONFLICT (workspace_id, name) DO UPDATE
           SET entity_type = COALESCE($3, entities.entity_type),
               description = COALESCE(EXCLUDED.description, entities.description)
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(name)
    .bind(entity_type)
    .bind(description)
    .fetch_one(pool)
    .await
}

/// Replace one chunk's extracted edges for one model, IN ONE WORKSPACE, and
/// mark the chunk extracted for that workspace — atomically.
///
/// Runs in ONE transaction: DELETE this workspace's existing edges for this
/// chunk+model, INSERT the new set, then write the `(workspace_id,
/// content_hash, model_id)` row in `chunk_extractions`. A crash mid-write
/// rolls the whole thing back, so a workspace's EDGES for a chunk and its
/// "done" marker always agree — never marked-but-edgeless.
///
/// SCOPE OF THAT GUARANTEE — edges and the marker, nothing wider. The caller
/// (`noted_index::graph_write::apply_extraction`) resolves entity rows via
/// `resolve_entity` on the POOL, before and outside this transaction, so a
/// full extraction is NOT one atomic unit. A failure between the entity
/// resolutions and this call leaves entities already inserted — and any
/// `entity_type` reclassification already applied — with no edges and no
/// marker. That is deliberately not widened here: it self-heals (the chunk is
/// still pending, the next pass re-resolves the same entities idempotently and
/// writes the edges) and loses no data; the residue is orphan entity nodes,
/// which is the same standing gap as
/// `orphan_entities_survive_an_edit_with_zero_live_edges_a_known_m2b_gap` and
/// is tracked as an M2b-1 prerequisite.
///
/// The marker lives INSIDE this transaction because it is per-workspace, the
/// same granularity as the edge write. (It was briefly a separate
/// `mark_extracted` call, written once after every workspace's edges had
/// landed. That existed only to work around a marker keyed on
/// `(content_hash, model_id)` with no workspace column: marking inside the
/// transaction would then have removed a SHARED chunk from the queue as soon
/// as the FIRST workspace was written, stranding the rest. Migration
/// `0008_chunk_extractions_workspace.sql` gave the marker a `workspace_id`,
/// which removes that constraint entirely — and makes the transactional
/// version strictly safer, since a crash between workspaces now leaves the
/// unwritten ones legitimately pending instead of relying on the caller to
/// defer a global marker correctly.)
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

    // The marker, in the SAME transaction as the edges above. `ON CONFLICT DO
    // NOTHING` keeps a re-extraction (which legitimately rewrites the edges of
    // an already-marked chunk) a no-op on the marker rather than an error;
    // `extracted_at` therefore records the FIRST extraction for this
    // workspace, which is the useful reading — "since when has this workspace
    // had a graph for this chunk".
    sqlx::query(
        "INSERT INTO chunk_extractions (workspace_id, content_hash, model_id)
         VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
    )
    .bind(workspace_id)
    .bind(chunk_hash)
    .bind(model_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await
}

/// Distinct workspaces whose LIVE pages currently reference `content_hash`.
///
/// A chunk is content-addressed and globally shared (M1b): two workspaces
/// can each have a live page containing byte-identical text, and each such
/// workspace needs its OWN copy of the extraction (entities/edges are scoped
/// per-workspace — see `resolve_entity`'s docs). The extraction worker calls
/// this once per pending chunk to fan its single `extract()` call out to
/// every workspace that needs the result written.
///
/// "Live" here means EXACTLY what `pending_extraction` means by it, archived
/// pages included (i.e. excluded): a page whose `archived_at` is set is dead
/// content and does not earn a graph. The two definitions MUST agree — if
/// this returned fewer workspaces than the queue considers live, the chunk
/// would be polled forever and never marked for the missing workspace, and
/// `drain` would spin until `MAX_CONSECUTIVE_FAILURES`.
pub async fn workspaces_for_chunk(
    pool: &sqlx::PgPool,
    content_hash: &str,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT DISTINCT p.workspace_id
         FROM page_chunks pc
         JOIN pages p ON p.id = pc.page_id
         WHERE pc.content_hash = $1
           AND p.archived_at IS NULL",
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
///
/// The marker join is scoped by `ce.workspace_id = p.workspace_id`, not by
/// `content_hash`/`model_id` alone. That is what makes the unit of work
/// "(chunk, workspace)" rather than "chunk": a chunk stays pending while ANY
/// workspace with a live page referencing it still lacks its own graph, so a
/// workspace that adopts an already-extracted shared chunk later is queued
/// like any other. See `0008_chunk_extractions_workspace.sql`.
///
/// "Live" excludes archived pages (`p.archived_at IS NULL`), matching
/// `chunks::pending` and `workspaces_for_chunk`. One definition of live
/// across both queues.
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
           ON ce.content_hash = c.content_hash
          AND ce.model_id = $1
          AND ce.workspace_id = p.workspace_id
         WHERE ce.content_hash IS NULL
           AND p.archived_at IS NULL
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
///
/// The unit counted is a **(chunk, workspace) pair**, matching
/// `pending_extraction`'s unit of work. For a scoped call (`Some(id)`) this
/// is indistinguishable from counting chunks. For the unscoped CLI call it
/// means a chunk shared by two workspaces counts TWICE — correct, because it
/// is two graphs to write, and it is what keeps `extracted == total`
/// reachable once the queue is drained. Counting it once could report 100%
/// while a second workspace still had no graph.
///
/// Archived pages are excluded, matching `pending_extraction` — otherwise a
/// chunk only reachable from archived pages would sit in the denominator
/// forever, un-drainable, pinning progress below 100%.
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
             SELECT DISTINCT pc.content_hash, p.workspace_id
             FROM page_chunks pc
             JOIN pages p ON p.id = pc.page_id
             WHERE p.archived_at IS NULL
               AND ($2::uuid IS NULL OR p.workspace_id = $2)
         ) pc
         LEFT JOIN chunk_extractions ce
           ON ce.content_hash = pc.content_hash
          AND ce.model_id = $1
          AND ce.workspace_id = pc.workspace_id",
    )
    .bind(model_id)
    .bind(workspace_id)
    .fetch_one(pool)
    .await
}

/// Delete edges whose source chunk is no longer referenced by any LIVE page in
/// their own workspace.
///
/// # Why this is not covered by the clustering filter
///
/// `clusterable_edges_cte!` already EXCLUDES these at read time, so nothing
/// surfaces them and correctness never depended on this function. What the
/// filter cannot do is stop the rows accumulating: archive a page and its edges
/// sit there forever, invisible and permanent, growing with every edit anyone
/// ever makes. This is the storage half of the same problem.
///
/// The liveness test is deliberately IDENTICAL to the macro's — a chunk is live
/// for a workspace when a non-archived page OF THAT WORKSPACE references it.
/// Two content-identical pages in different tenants share a chunk (M1b), so
/// "some live page somewhere references this" is the wrong question and would
/// keep one tenant's edges alive on the strength of another tenant's page.
pub async fn reap_dead_edges(
    pool: &sqlx::PgPool,
    workspace_id: Option<Uuid>,
) -> Result<u64, sqlx::Error> {
    let deleted = sqlx::query(
        "DELETE FROM edges e
         WHERE ($1::uuid IS NULL OR e.workspace_id = $1)
           AND NOT EXISTS (
               SELECT 1
               FROM page_chunks pc
               JOIN pages p ON p.id = pc.page_id
               WHERE pc.content_hash = e.source_chunk_hash
                 AND p.workspace_id = e.workspace_id
                 AND p.archived_at IS NULL
           )",
    )
    .bind(workspace_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(deleted)
}

/// Delete entities that no edge names any more.
///
/// MUST RUN AFTER [`reap_dead_edges`], not before: an entity is orphaned by the
/// removal of the last edge that named it, so reaping entities first would find
/// nothing to do and leave exactly the nodes the edge sweep was about to
/// orphan. The two are one job in two statements, and `reap_graph` is the entry
/// point that gets the order right.
///
/// An entity with no edges is unreachable by every surface in the product:
/// clustering only sees entities via `clusterable_edges_cte!`, local search
/// anchors through edges, and there is no UI that lists entities directly. So
/// this deletes nothing anybody could observe — it reclaims storage, and stops
/// a future feature that DOES list entities from showing a graveyard.
pub async fn reap_orphan_entities(
    pool: &sqlx::PgPool,
    workspace_id: Option<Uuid>,
) -> Result<u64, sqlx::Error> {
    let deleted = sqlx::query(
        "DELETE FROM entities en
         WHERE ($1::uuid IS NULL OR en.workspace_id = $1)
           AND NOT EXISTS (
               SELECT 1 FROM edges e
               WHERE e.source_entity = en.id OR e.target_entity = en.id
           )",
    )
    .bind(workspace_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(deleted)
}

/// How much graph residue a sweep removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reaped {
    pub edges: u64,
    pub entities: u64,
}

/// Sweep dead edges and then the entities they orphaned, in that order.
///
/// Safe to run at any time and safe to interrupt: both statements are ordinary
/// deletes of rows nothing can reach, so a crash between them leaves the
/// entities to be collected by the next sweep rather than leaving anything
/// inconsistent.
pub async fn reap_graph(
    pool: &sqlx::PgPool,
    workspace_id: Option<Uuid>,
) -> Result<Reaped, sqlx::Error> {
    let edges = reap_dead_edges(pool, workspace_id).await?;
    let entities = reap_orphan_entities(pool, workspace_id).await?;
    Ok(Reaped { edges, entities })
}

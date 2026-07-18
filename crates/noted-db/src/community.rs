//! Community repository: atomic partition swaps, member-set hashing, and the
//! churn counter that decides when the cold path runs.
//!
//! Like `graph`, this module deals ONLY in primitives (`Uuid`, `String`,
//! tuples). The clustering itself — Louvain, the canonical node ordering, the
//! hot-path reassignment — lives in `noted-index`, which already depends on
//! `noted-db`; `noted-db` must never depend back on it. So a partition arrives
//! here as plain `(level, member ids)` tuples, not as any clusterer type.
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// SHA-256 over the community's members, SORTED and de-duplicated.
///
/// Sorting is the entire point, and it buys two independent invariances:
///
///   * **Member order.** A cold run that re-derives the same community may emit
///     its members in any order. An order-sensitive hash would call that a
///     changed community.
///   * **Community relabelling.** The hash never sees a community id or its
///     position in the partition, only its members — so re-running the
///     clusterer and getting the same set-partition under different labels
///     leaves every hash identical. This was measured during M2b research: raw
///     label vectors compare unequal after a pure relabel while member-set
///     hashes compare equal.
///
/// That second one is load-bearing for cost, not just tidiness. The hash is what
/// `swap_partition` matches on to preserve a community's row (and therefore its
/// summary — see `swap_partition`), and what M2b-3 compares to decide whether a
/// summary is still valid. Get it wrong and every partition swap invalidates
/// every community and triggers a full summary regeneration storm, which is a
/// model call per community for a partition that did not actually move.
///
/// Duplicates are collapsed because the stored membership cannot represent them
/// (`community_members`' primary key is `(community_id, entity_id)`), so a hash
/// that counted them would describe a set the database can never hold.
///
/// Ids are joined with a unit separator. UUID text is fixed-width so no
/// ambiguity is actually possible here, but the separator costs nothing and
/// matches `noted_crdt::project`'s hashing convention.
pub fn member_set_hash(entity_ids: &[Uuid]) -> String {
    let mut sorted: Vec<Uuid> = entity_ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut h = Sha256::new();
    for id in sorted {
        h.update(id.as_hyphenated().to_string().as_bytes());
        h.update([0x1f]);
    }
    format!("{:x}", h.finalize())
}

/// Replace a workspace's ENTIRE partition, in ONE transaction.
///
/// # The argument shape, and why
///
/// `partition` is `&[(level, member entity ids)]` — one tuple per community,
/// with no community id and no hash. Three consequences, all deliberate:
///
///   * **No caller-supplied hash.** `member_set_hash` is computed here, from the
///     members actually being written. A caller that passed both could pass a
///     hash that disagrees with the membership, and every downstream decision
///     (row preservation, summary validity) trusts that hash — so the
///     inconsistency would be silent and would surface as summaries describing
///     the wrong entities.
///   * **No caller-supplied community id.** Identity is not the caller's to
///     assign; it is derived from membership (see below). A clusterer's integer
///     labels are arbitrary and change run to run, which is exactly what must
///     NOT leak into stored identity.
///   * **`level` is explicit** because Louvain is hierarchical and the same
///     member set can legitimately exist at two levels.
///
/// # Atomicity
///
/// All-or-nothing. Replacing a partition necessarily means deleting the old
/// communities before the new ones exist, so if the delete and the inserts were
/// not one transaction, a failure between them would leave the workspace with NO
/// partition — the graph unqueryable until the next cold run, which is precisely
/// the outage the always-available hot/cold design exists to prevent. A failure
/// anywhere in here rolls back to the PREVIOUS partition, intact and queryable.
/// See `tests/community.rs::a_swap_that_fails_midway_leaves_the_previous_partition_intact`,
/// which forces the failure rather than asserting on a swap that succeeded.
///
/// # Identity is preserved across a swap when membership is unchanged
///
/// A community whose `(level, member_set_hash)` already exists in this workspace
/// keeps its EXISTING ROW — same `id`, same `created_at`. This is not an
/// optimisation. `community_summaries` is keyed by `community_id` and cascades
/// on delete, so the naive "delete every community, insert the new ones" swap
/// would destroy every summary on every cold run, and M2b-3's "unchanged
/// membership ⇒ zero summariser calls" would be unreachable no matter how good
/// the hash is. The `UNIQUE (workspace_id, level, member_set_hash)` constraint in
/// `0009_communities.sql` is what makes the match unambiguous, and it is sound
/// because a partition's communities are disjoint.
///
/// Members are nevertheless deleted and re-inserted even for a preserved row.
/// It is a no-op by construction (equal hashes mean equal member sets) and costs
/// nothing at these sizes, but it means the table cannot drift out of agreement
/// with the hash that names it, whatever else may have touched it.
///
/// # Tenancy
///
/// Every statement carries `workspace_id`. Clustering workspace A cannot delete,
/// re-create, or renumber any of B's communities — which matters beyond the row
/// counts, since B's summaries are keyed by B's community ids.
pub async fn swap_partition(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    partition: &[(i32, Vec<Uuid>)],
) -> Result<(), sqlx::Error> {
    let levels: Vec<i32> = partition.iter().map(|(level, _)| *level).collect();
    let hashes: Vec<String> = partition
        .iter()
        .map(|(_, members)| member_set_hash(members))
        .collect();

    let mut tx = pool.begin().await?;

    // Drop this workspace's communities that the new partition does not
    // contain. Their members and summaries go with them (ON DELETE CASCADE) —
    // correct, because a summary describes a membership that no longer exists.
    // An empty `partition` is legitimate (a workspace with no clusterable
    // graph) and clears the workspace; UNNEST over empty arrays yields no rows,
    // so NOT EXISTS holds for every community and the DELETE is unconditional.
    sqlx::query(
        "DELETE FROM communities c
         WHERE c.workspace_id = $1
           AND NOT EXISTS (
             SELECT 1 FROM UNNEST($2::int[], $3::text[]) AS k(level, member_set_hash)
             WHERE k.level = c.level AND k.member_set_hash = c.member_set_hash
           )",
    )
    .bind(workspace_id)
    .bind(&levels)
    .bind(&hashes)
    .execute(&mut *tx)
    .await?;

    for ((level, members), hash) in partition.iter().zip(&hashes) {
        // `DO UPDATE SET level = EXCLUDED.level` is a deliberate no-op write:
        // it re-sets the column to the value it already has, purely so that
        // RETURNING yields the id on the conflict path too. `DO NOTHING`
        // returns no row when the community already exists, which is the case
        // that matters most here — the preserved one.
        let community_id: Uuid = sqlx::query_scalar(
            "INSERT INTO communities (workspace_id, level, member_set_hash)
             VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, level, member_set_hash) DO UPDATE
               SET level = EXCLUDED.level
             RETURNING id",
        )
        .bind(workspace_id)
        .bind(level)
        .bind(hash)
        .fetch_one(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM community_members WHERE community_id = $1")
            .bind(community_id)
            .execute(&mut *tx)
            .await?;

        if !members.is_empty() {
            // De-duplicated to match `member_set_hash`, which hashes the set:
            // a repeated id would otherwise violate the primary key and abort a
            // swap over a membership the hash considers perfectly ordinary.
            let mut unique: Vec<Uuid> = members.clone();
            unique.sort_unstable();
            unique.dedup();

            sqlx::query(
                "INSERT INTO community_members (community_id, entity_id)
                 SELECT $1, e FROM UNNEST($2::uuid[]) AS x(e)",
            )
            .bind(community_id)
            .bind(&unique)
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await
}

/// Add `n` to a workspace's churn counter — edges changed since its last full
/// clustering run.
///
/// ACCUMULATES rather than overwrites (`edges_changed + EXCLUDED.edges_changed`).
/// The cold path fires on a threshold, so an overwriting counter would keep
/// resetting to the size of the most recent edit and the threshold would never
/// be reached — many small edits are exactly the case this is meant to catch.
///
/// Upserts the row, so a workspace needs no churn row until something actually
/// changes.
pub async fn bump_churn(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    n: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO graph_churn (workspace_id, edges_changed)
         VALUES ($1, $2)
         ON CONFLICT (workspace_id) DO UPDATE
           SET edges_changed = graph_churn.edges_changed + EXCLUDED.edges_changed",
    )
    .bind(workspace_id)
    .bind(n)
    .execute(pool)
    .await?;
    Ok(())
}

/// THE ONE DEFINITION OF "CLUSTERABLE". Read the whole comment before touching it.
///
/// A common-table-expression body naming `clusterable_edges` — the edges that
/// clustering is allowed to see. Every query below splices this in verbatim,
/// which is the point: the cold path and the hot path MUST agree, exactly, on
/// which graph they are operating over.
///
/// # Why a filter and not a reaper
///
/// Nothing in this system ever removes graph nodes or edges. An entity whose
/// edges all vanished after a page edit survives with zero live edges
/// (`noted-index/tests/incremental.rs`), and archiving a page — the product's
/// delete — leaves its `entities` and `edges` completely intact, with nothing
/// that will ever revisit them. Clustering over that raw table would put dead
/// and user-deleted content into communities, which M2b-3 would then summarise
/// and M2c would then retrieve.
///
/// The DECISION (M2b-1 prerequisite, option (b)) is to filter here rather than
/// build a reaper: it fixes the correctness problem without introducing a
/// destructive code path, and it is reversible — un-archiving a page brings its
/// entities straight back into the next cold run. Storage reclamation waits
/// until it measures as a problem.
///
/// # Why it MUST be one string
///
/// If the two paths disagree about liveness, the §5 convergence property is
/// comparing two different graphs, and it fails in ways that look exactly like
/// Louvain non-determinism — you would spend hours debugging the clusterer for
/// a bug in a WHERE clause. This codebase has produced four separate data bugs
/// from two code paths disagreeing about what "live" means (see the M2a
/// standing rule in `.superpowers/sdd/progress.md`), and the fix each time was
/// to make one definition serve both. So: one const, spliced, never re-typed.
///
/// # `p.workspace_id = e.workspace_id` is load-bearing
///
/// `source_chunk_hash` is a GLOBAL, content-addressed key — two workspaces
/// holding byte-identical text share one `chunks` row (M1b). An edge, by
/// contrast, belongs to exactly one workspace (0007). So liveness must be
/// judged against THAT workspace's own pages. Without the correlation,
/// workspace A archiving its only page referencing a chunk would leave A's
/// edges "live" as long as some UNRELATED workspace B still had a live page
/// with the same text — one tenant's content keeping another tenant's deleted
/// content in its communities. This is the M2a trap in its exact original
/// shape: a global content key gating a per-tenant decision.
///
/// # Binding contract
///
/// `$1` is the workspace id, in every query that splices this. Callers must
/// bind it first and must not renumber it.
///
/// Note it does NOT filter on `model_id`: a workspace's graph is the UNION of
/// what every extraction model contributed, because `entities` carries no
/// model and the two models' edges genuinely describe the same nodes. Running
/// two extraction models at once would therefore cluster over the merged
/// graph. Recorded as a deliberate choice, not an oversight; revisit if
/// side-by-side extraction models ever become a real workflow.
///
/// # Why a macro rather than a `const &str`
///
/// sqlx 0.9 accepts only `&'static str` as SQL (its `SqlSafeStr` bound exists
/// precisely to stop runtime-assembled query text). A `const` spliced with
/// `format!` produces a `String` and is rejected — correctly. Expanding to a
/// string LITERAL lets `concat!` build each query at compile time, so every
/// query below is still a `'static` literal that no runtime value can reach,
/// and the shared definition costs nothing at run time.
macro_rules! clusterable_edges_cte {
    () => {
        "
    clusterable_edges AS (
        SELECT e.source_entity, e.target_entity, e.weight
        FROM edges e
        WHERE e.workspace_id = $1
          AND EXISTS (
              SELECT 1
              FROM page_chunks pc
              JOIN pages p ON p.id = pc.page_id
              WHERE pc.content_hash = e.source_chunk_hash
                AND p.workspace_id = e.workspace_id
                AND p.archived_at IS NULL
          )
    )"
    };
}

/// The workspace's clusterable entity graph: `(nodes, edges)`.
///
/// `nodes` is `(entity_id, name)` **ordered by `name` ascending** — the
/// canonical order, and the caller is expected to use position in this vector
/// as the node index it hands to the clusterer.
///
/// **The sort key is `name`, NEVER `id`.** `entities.id` is
/// `gen_random_uuid()`: random per insert, so id-order is stable within one
/// database but NOT across a rebuild, and §5's incremental-vs-full-rebuild
/// comparison would order the two graphs differently and diverge for reasons
/// that have nothing to do with the algorithm. `UNIQUE (workspace_id, name)`
/// makes `name` the stable natural key. Node-index assignment order IS the
/// non-determinism in every community-detection algorithm, so this ORDER BY is
/// the entire mitigation — see `noted_index::louvain`'s module docs.
///
/// A node appears iff it is an endpoint of at least one clusterable edge, so
/// "clusterable entity" is derived from the shared edge definition rather than
/// stated a second time. Orphans and archived-only entities are therefore absent
/// by construction, not by a second filter that could drift.
///
/// `en.workspace_id = $1` here is DEFENCE IN DEPTH, not the load-bearing scope,
/// and is documented as such rather than left to imply a hot path — the same
/// correction M2a had to make to `replace_chunk_edges`'s ON CONFLICT clause.
/// Measured: deleting this predicate kills no test, because the `EXISTS` below
/// already restricts nodes to endpoints of THIS workspace's edges, and
/// `resolve_entity` is per-workspace so an edge can only ever point at entities
/// of its own workspace. It stays because it is index-backed and free, and
/// because it is the one thing that would contain a future writer that broke
/// that invariant. The scope that IS load-bearing is `e.workspace_id = $1`
/// inside the shared edge definition; removing THAT does fail tests
/// (`the_cold_path_fires_only_once_churn_crosses_the_threshold`, via a churn
/// denominator inflated by every other tenant's edges).
///
/// `edges` are undirected and aggregated: each unordered pair appears ONCE with
/// its summed weight, so relation type, provenance chunk and direction are all
/// collapsed into a single scalar affinity. Pairs are canonicalised with
/// `LEAST`/`GREATEST` so `A->B` and `B->A` merge instead of arriving as two
/// entries whose order would depend on row order.
pub async fn clusterable_graph(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
) -> Result<(Vec<(Uuid, String)>, Vec<(Uuid, Uuid, f64)>), sqlx::Error> {
    let nodes: Vec<(Uuid, String)> = sqlx::query_as(concat!(
        "WITH ",
        clusterable_edges_cte!(),
        " SELECT en.id, en.name
          FROM entities en
          WHERE en.workspace_id = $1
            AND EXISTS (
              SELECT 1 FROM clusterable_edges ce
              WHERE ce.source_entity = en.id OR ce.target_entity = en.id
            )
          ORDER BY en.name ASC"
    ))
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;

    let edges: Vec<(Uuid, Uuid, f64)> = sqlx::query_as(concat!(
        "WITH ",
        clusterable_edges_cte!(),
        " SELECT LEAST(ce.source_entity, ce.target_entity)    AS a,
                 GREATEST(ce.source_entity, ce.target_entity) AS b,
                 SUM(ce.weight)::double precision             AS w
          FROM clusterable_edges ce
          GROUP BY 1, 2
          ORDER BY 1, 2"
    ))
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;

    Ok((nodes, edges))
}

/// How many clusterable edges the workspace has — the denominator the churn
/// threshold is a fraction OF.
///
/// Counts the same aggregated unordered pairs `clusterable_graph` returns, so
/// "5% of edges" means 5% of the edges the cold path would actually cluster,
/// not 5% of `edges` rows (which counts each relation and each provenance chunk
/// separately and would therefore make the threshold fire far too late).
pub async fn clusterable_edge_count(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(concat!(
        "WITH ",
        clusterable_edges_cte!(),
        " SELECT count(*) FROM (
             SELECT DISTINCT LEAST(ce.source_entity, ce.target_entity),
                             GREATEST(ce.source_entity, ce.target_entity)
             FROM clusterable_edges ce
          ) x"
    ))
    .bind(workspace_id)
    .fetch_one(pool)
    .await
}

/// The community of `entity_id`'s most strongly-connected clusterable
/// neighbour — the hot path's whole decision, in one O(degree) query.
///
/// Neighbours are ranked by summed clusterable edge weight, descending, then by
/// `entities.name` ascending. The name tie-break is not decoration: two
/// neighbours at equal weight are a genuine tie, and without a deterministic
/// second key the answer would depend on Postgres' row order and the hot path
/// would be irreproducible. Same discipline as the clusterer's
/// strictly-greater-plus-epsilon comparison.
///
/// Neighbours with NO community yet are skipped rather than ending the search:
/// "this neighbour has not been clustered" is not evidence about where the
/// entity belongs, so the next-strongest neighbour that HAS been clustered is a
/// strictly better guess than giving up. Returns `None` only when no clusterable
/// neighbour of `entity_id` belongs to any of this workspace's communities.
///
/// Uses the SAME `CLUSTERABLE_EDGES` definition the cold path uses. That is the
/// single most important line in this module.
pub async fn strongest_clustered_neighbour(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    entity_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(concat!(
        "WITH ",
        clusterable_edges_cte!(),
        ", neighbours AS (
             SELECT CASE WHEN ce.source_entity = $2 THEN ce.target_entity
                         ELSE ce.source_entity END       AS neighbour,
                    SUM(ce.weight)::double precision     AS w
             FROM clusterable_edges ce
             WHERE ce.source_entity = $2 OR ce.target_entity = $2
             GROUP BY 1
         )
         SELECT cm.community_id
         FROM neighbours n
         JOIN entities ne          ON ne.id = n.neighbour
         JOIN community_members cm ON cm.entity_id = n.neighbour
         JOIN communities c        ON c.id = cm.community_id AND c.workspace_id = $1
         WHERE n.neighbour <> $2
         ORDER BY n.w DESC, ne.name ASC
         LIMIT 1"
    ))
    .bind(workspace_id)
    .bind(entity_id)
    .fetch_optional(pool)
    .await
}

/// Move one entity into `community_id`, maintaining every invariant the swap
/// depends on. THE HOT PATH'S ONLY WRITE.
///
/// In ONE transaction: drop the entity's membership in every community of this
/// workspace, add it to `community_id`, and re-derive `member_set_hash` for
/// both the community it left and the one it joined.
///
/// # Why the hash is maintained here rather than left to the next cold run
///
/// `member_set_hash` is what `swap_partition` matches on to PRESERVE a
/// community's row, and therefore its summary. A hot-path move that left the
/// hash describing the old membership would invert that: when the next cold run
/// CONFIRMS the move (the common case — the hot path is meant to approximate,
/// not to guess wildly), the stored hash would fail to match the recomputed one,
/// the row would be dropped and re-created, and a summary would be regenerated
/// for a community that never actually changed. Keeping the hash true to the
/// membership makes the confirmation free, which is the entire economic argument
/// for hashing member sets at all.
///
/// # A community emptied by the move is DELETED
///
/// Two reasons, and the second is a hard constraint rather than tidiness. A
/// community with no members describes nothing and cannot be summarised. And
/// `UNIQUE (workspace_id, level, member_set_hash)` would be violated the moment
/// a SECOND community emptied, since both would hash the empty set — a
/// constraint violation aborting a hot-path write that is supposed to be
/// infallible bookkeeping. Non-empty communities can never collide: a
/// partition's communities are disjoint, so no two share a member set.
/// Cascades take the members and the summary with it, which is correct — the
/// summary described a membership that no longer exists.
pub async fn reassign_entity(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    entity_id: Uuid,
    community_id: Uuid,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Which communities this write touches: everything the entity is leaving,
    // plus the one it is joining. Collected BEFORE the delete, because after it
    // there is nothing left to identify the source community by.
    let mut touched: Vec<Uuid> = sqlx::query_scalar(
        "SELECT cm.community_id
         FROM community_members cm
         JOIN communities c ON c.id = cm.community_id
         WHERE cm.entity_id = $1 AND c.workspace_id = $2",
    )
    .bind(entity_id)
    .bind(workspace_id)
    .fetch_all(&mut *tx)
    .await?;
    touched.push(community_id);
    touched.sort_unstable();
    touched.dedup();

    sqlx::query(
        "DELETE FROM community_members cm
         USING communities c
         WHERE cm.community_id = c.id
           AND cm.entity_id = $1
           AND c.workspace_id = $2",
    )
    .bind(entity_id)
    .bind(workspace_id)
    .execute(&mut *tx)
    .await?;

    // Scoped to the workspace: a caller that passed another tenant's community
    // id must write nothing at all, rather than quietly moving an entity across
    // the tenancy boundary.
    sqlx::query(
        "INSERT INTO community_members (community_id, entity_id)
         SELECT c.id, $2 FROM communities c
         WHERE c.id = $1 AND c.workspace_id = $3
         ON CONFLICT DO NOTHING",
    )
    .bind(community_id)
    .bind(entity_id)
    .bind(workspace_id)
    .execute(&mut *tx)
    .await?;

    for id in touched {
        let members: Vec<Uuid> =
            sqlx::query_scalar("SELECT entity_id FROM community_members WHERE community_id = $1")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;

        if members.is_empty() {
            sqlx::query("DELETE FROM communities WHERE id = $1 AND workspace_id = $2")
                .bind(id)
                .bind(workspace_id)
                .execute(&mut *tx)
                .await?;
            continue;
        }

        sqlx::query(
            "UPDATE communities SET member_set_hash = $2 WHERE id = $1 AND workspace_id = $3",
        )
        .bind(id)
        .bind(member_set_hash(&members))
        .bind(workspace_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await
}

/// Zero the churn counter and stamp `last_full_run_at` — what a completed cold
/// run does, and nothing else may do.
///
/// One statement, so the two can never disagree: a counter zeroed without a
/// stamp claims a full run that never happened, and a stamp without a zeroed
/// counter re-fires the cold path immediately on the next edit. `now()` is the
/// database's clock, the same one `churn` reads back, so no client-skew
/// comparison is ever involved.
pub async fn mark_full_run(pool: &sqlx::PgPool, workspace_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO graph_churn (workspace_id, edges_changed, last_full_run_at)
         VALUES ($1, 0, now())
         ON CONFLICT (workspace_id) DO UPDATE
           SET edges_changed = 0, last_full_run_at = now()",
    )
    .bind(workspace_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// `(edges_changed, last_full_run_at)` for a workspace.
///
/// A workspace with no row reads as `(0, None)` rather than an error: it has
/// changed no edges and has never had a full run, which is the truthful answer
/// and means the cold-path scheduler can ask about every workspace, including
/// ones created a moment ago, without initialising anything first.
pub async fn churn(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
) -> Result<(i64, Option<DateTime<Utc>>), sqlx::Error> {
    let row: Option<(i64, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT edges_changed, last_full_run_at FROM graph_churn WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or((0, None)))
}

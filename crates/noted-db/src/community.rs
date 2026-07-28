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

/// Replace a workspace's ENTIRE partition, in ONE transaction. Returns the
/// number of communities STORED.
///
/// # What `partition` is allowed to be, and what happens when it is not
///
/// A partition is a set of disjoint, non-empty member sets, and that is what
/// Louvain hands over. This function is nevertheless public and takes a plain
/// slice, so it normalises rather than trusting:
///
///   * an EMPTY member set is dropped — the same rule `reassign_entity` applies
///     when a move empties a community, and for the same reasons: it describes
///     nothing, cannot be summarised, and two of them cannot coexist anyway
///     because both hash the empty set;
///   * a member set that REPEATS one already seen at the same level is dropped,
///     because identity here is `(level, member_set_hash)` and the two entries
///     name one community.
///
/// The return value is what makes that safe. Identity being derived from
/// membership means the `ON CONFLICT` below would have absorbed both cases
/// regardless — storing one row per distinct key while the caller counted
/// entries. `CommunityWorker::cold_run` reported exactly that count, so a
/// caller could be told it had stored more communities than exist. Normalising
/// up front and returning the normalised size makes the two agree by
/// construction instead of by the caller's good behaviour.
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
) -> Result<usize, sqlx::Error> {
    // NORMALISE FIRST, and return the normalised size — see "What `partition` is
    // allowed to be" above. Empty member sets are dropped; entries that repeat a
    // `(level, member_set_hash)` already seen are dropped. Both were previously
    // absorbed by the `ON CONFLICT` below, which stored one row per DISTINCT
    // key while the caller was told it had stored one per ENTRY.
    let mut levels: Vec<i32> = Vec::with_capacity(partition.len());
    let mut hashes: Vec<String> = Vec::with_capacity(partition.len());
    let mut normalised: Vec<(i32, &Vec<Uuid>)> = Vec::with_capacity(partition.len());
    for (level, members) in partition {
        if members.is_empty() {
            continue;
        }
        let hash = member_set_hash(members);
        if levels
            .iter()
            .zip(&hashes)
            .any(|(l, h)| *l == *level && *h == hash)
        {
            continue;
        }
        levels.push(*level);
        hashes.push(hash);
        normalised.push((*level, members));
    }

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

    for ((level, members), hash) in normalised.iter().zip(&hashes) {
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

        // De-duplicated to match `member_set_hash`, which hashes the set: a
        // repeated id would otherwise violate the primary key and abort a swap
        // over a membership the hash considers perfectly ordinary. `members` is
        // non-empty by construction — the normalisation above dropped the empty
        // ones — so there is no emptiness case left to guard.
        let mut unique: Vec<Uuid> = (*members).clone();
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

    tx.commit().await?;
    Ok(normalised.len())
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
/// from two code paths disagreeing about what "live" means, and the fix each
/// time was to make one definition serve both. So: one const, spliced, never
/// re-typed.
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
/// # Columns
///
/// `source_entity`, `target_entity`, `weight`, `source_chunk_hash`. The last was
/// added for M2c's local search, whose FIRST step is "which entities does a seed
/// CHUNK anchor" — a question that needs the provenance hash and cannot be
/// answered from the endpoint pair alone. The alternative considered and
/// rejected was to join `edges` directly for that one step and test the pair
/// against `clusterable_edges` with an `EXISTS`: that would have let an ARCHIVED
/// page's chunk seed the traversal whenever some *other*, live chunk happened to
/// connect the same two entities — a second, weaker definition of live, which is
/// the exact bug class this macro exists to prevent. Adding a column is safe
/// because every consumer selects named columns (no `SELECT *`, no `UNION` over
/// the CTE), so the extra column is invisible to the five existing ones.
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
    // ALWAYS takes a materialisation hint, so every call site states which kind
    // of consumer it is rather than inheriting a default nobody thought about.
    //
    // `""` — let Postgres decide. What every consumer that SCANS the whole set
    // wants: the clusterer, the churn count, the stats. The set is computed
    // once and read once, and materialising it is right.
    //
    // `"NOT MATERIALIZED"` for consumers that JOIN AGAINST the set instead of
    // scanning it.
    //
    // The recursive traversal in `graph_search` references this CTE three
    // times, which makes Postgres materialise it — and a materialised CTE
    // cannot be indexed. Measured at 4k edges: 69,300 rows discarded by the
    // join filter per query, because each frontier row rescans the whole
    // materialised set. Cost is O(frontier x live edges), and the depth cap
    // does NOT bound it, because the cost is in the rescan rather than the
    // depth.
    //
    // Inlining lets the planner push `ce.source_entity = w.entity_id` down into
    // `edges` and use `edges_source_entity_idx`.
    //
    // This is a HINT PARAMETER rather than a second copy of the CTE on purpose:
    // four data-loss bugs in this codebase came from two queries disagreeing
    // about what "live" meant, so the definition stays in one place and only
    // the materialisation strategy varies.
    ($hint:literal) => {
        concat!("
    clusterable_edges AS ", $hint, " (
        SELECT e.source_entity, e.target_entity, e.weight, e.source_chunk_hash
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
    )")
    };
}

// Re-exported crate-internally so `stats` can splice the SAME definition rather
// than paraphrase it. The dashboard's `entities`/`edges` counts and the
// clusterer must agree on which edges exist for the same reason the hot and cold
// paths must: two texts saying the same thing today is not a guarantee.
pub(crate) use clusterable_edges_cte;

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
        clusterable_edges_cte!(""),
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
        clusterable_edges_cte!(""),
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
        clusterable_edges_cte!(""),
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
/// Splices the SAME `clusterable_edges_cte!` macro the cold path splices — the
/// hot path and the cold path must never disagree about which edges exist, or
/// the approximation is approximating a different graph from the one it will be
/// corrected against. That is the single most important line in this module.
/// (This referred to a `CLUSTERABLE_EDGES` const until sqlx 0.9's `SqlSafeStr`
/// bound forced the shared definition to become a macro; see the note there.)
pub async fn strongest_clustered_neighbour(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    entity_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(concat!(
        "WITH ",
        clusterable_edges_cte!(""),
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
///
/// # Tenancy is checked on BOTH ids, and a foreign one is a silent no-op
///
/// A `community_members` row names a community AND an entity, so `workspace_id`
/// has to gate both or the boundary is only half closed. It was half closed
/// until a review probed the unguarded side: the insert scoped `communities` and
/// nothing scoped `entities`, so `reassign_entity(A, entity_of_B,
/// community_of_A)` returned `Ok` having written a cross-tenant membership. Both
/// are guarded now, and a call naming either a foreign community or a foreign
/// entity writes nothing and reports success — see the note at the insert for
/// why declining beats raising on this path.
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

    // BOTH ids are scoped to the workspace, and both directions are load-bearing.
    // A membership row names a community and an entity, so it can cross the
    // tenancy boundary from either end: another tenant's COMMUNITY (guarded by
    // `c.workspace_id`) or another tenant's ENTITY (guarded by the join to
    // `entities`). Guarding only the community — which is what this did until a
    // review probed the other side — let `reassign_entity(A, entity_of_B,
    // community_of_A)` insert a cross-tenant row and return `Ok`. Either foreign
    // id now writes nothing at all.
    //
    // Declining rather than erroring, deliberately: `CommunityWorker::hot_reassign`
    // treats each reassignment as infallible bookkeeping, so a raise here would
    // abort a hot-path run over a write this statement can simply not perform.
    sqlx::query(
        "INSERT INTO community_members (community_id, entity_id)
         SELECT c.id, en.id
         FROM communities c
         JOIN entities en ON en.id = $2 AND en.workspace_id = $3
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

/// One community's summary, as global search consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryCandidate {
    pub community_id: Uuid,
    pub summary: String,
    /// `'valid'` or `'stale_usable'` (M2b §2.2). Carried rather than filtered on
    /// because global search USES a stale summary and separately asks for its
    /// regeneration — dropping it here would make that impossible.
    pub state: String,
    pub member_count: i64,
}

/// What `summaries_for_search` found, and what it had to leave out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarySelection {
    pub candidates: Vec<SummaryCandidate>,
    /// Communities in this workspace with NO usable summary under `model_id`.
    ///
    /// RETURNED, NOT DISCARDED. A global answer computed over 3 of 40
    /// communities is not wrong, but presenting it as though it covered the
    /// workspace would be — and the caller cannot recover this number later.
    pub skipped_unsummarised: i64,
}

/// Select the communities a global search should map over.
///
/// # Selection is by SIZE, and that is the milestone's weakest point
///
/// The principled selection is semantic: embed the question, embed each summary,
/// rank by similarity. That needs a THIRD embedding space (chunks have one,
/// entities deliberately do not — see the M2c design §2.1) plus its own backfill
/// queue and index. Until that exists this ranks by member count, i.e. "biggest
/// themes first", which is a proxy for importance and NOT for relevance to the
/// question. Recorded plainly here and in the design's risk table rather than
/// dressed up: a question about a niche topic will map over the workspace's
/// largest communities, which may not include it.
///
/// # `model_id` filters, and mismatches count as skipped
///
/// A summary written by a different summariser is prose about the right
/// community from the wrong model. M2b already treats a model change as a full
/// regeneration (`summary_worker::classify`), so including such rows here would
/// contradict that. They are excluded and counted in `skipped_unsummarised`,
/// which is exactly what they are from this search's point of view.
pub async fn summaries_for_search(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    model_id: &str,
    limit: i64,
) -> Result<SummarySelection, sqlx::Error> {
    let limit = limit.clamp(1, 50);

    let candidates: Vec<SummaryCandidate> = sqlx::query_as::<_, (Uuid, String, String, i64)>(
        "SELECT c.id,
                s.summary,
                s.state,
                count(cm.entity_id) AS member_count
         FROM communities c
         JOIN community_summaries s ON s.community_id = c.id AND s.model_id = $2
         LEFT JOIN community_members cm ON cm.community_id = c.id
         WHERE c.workspace_id = $1
         GROUP BY c.id, s.summary, s.state, s.created_at
         ORDER BY member_count DESC, s.created_at DESC, c.id
         LIMIT $3",
    )
    .bind(workspace_id)
    .bind(model_id)
    .bind(limit)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(
        |(community_id, summary, state, member_count)| SummaryCandidate {
            community_id,
            summary,
            state,
            member_count,
        },
    )
    .collect();

    let skipped_unsummarised: i64 = sqlx::query_scalar(
        "SELECT count(*)
         FROM communities c
         WHERE c.workspace_id = $1
           AND NOT EXISTS (
               SELECT 1 FROM community_summaries s
               WHERE s.community_id = c.id AND s.model_id = $2
           )",
    )
    .bind(workspace_id)
    .bind(model_id)
    .fetch_one(pool)
    .await?;

    Ok(SummarySelection {
        candidates,
        skipped_unsummarised,
    })
}


// ------------------------------------------------- semantic selection (M6-3) --

/// Store (or replace) the embedding of a community's summary.
///
/// `summary_hash` is the hash of the text the vector was made from, so a
/// regenerated summary can be detected and re-embedded rather than left with a
/// vector describing prose that no longer exists.
/// Store a summary embedding, letting POSTGRES compute the summary hash.
///
/// The hash must equal the `md5(s.summary)` that
/// [`summaries_needing_embedding`] compares against, or the row looks
/// permanently stale and is re-embedded on every pass forever. Computing it
/// here, in the same expression the queue uses, makes that agreement
/// structural rather than something two implementations have to keep matching
/// — a Rust md5 and Postgres's could differ over nothing more than text
/// encoding and the symptom would be an invisible infinite loop.
pub async fn store_summary_embedding_for_text(
    pool: &sqlx::PgPool,
    community_id: Uuid,
    model_id: &str,
    summary: &str,
    embedding: &[f32],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO community_summary_embeddings
             (community_id, model_id, summary_hash, embedding)
         VALUES ($1, $2, md5($3), $4)
         ON CONFLICT (community_id, model_id)
         DO UPDATE SET summary_hash = EXCLUDED.summary_hash,
                       embedding = EXCLUDED.embedding,
                       created_at = now()",
    )
    .bind(community_id)
    .bind(model_id)
    .bind(summary)
    .bind(pgvector::Vector::from(embedding.to_vec()))
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn store_summary_embedding(
    pool: &sqlx::PgPool,
    community_id: Uuid,
    model_id: &str,
    summary_hash: &str,
    embedding: &[f32],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO community_summary_embeddings
             (community_id, model_id, summary_hash, embedding)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (community_id, model_id)
         DO UPDATE SET summary_hash = EXCLUDED.summary_hash,
                       embedding = EXCLUDED.embedding,
                       created_at = now()",
    )
    .bind(community_id)
    .bind(model_id)
    .bind(summary_hash)
    .bind(pgvector::Vector::from(embedding.to_vec()))
    .execute(pool)
    .await?;
    Ok(())
}

/// Workspaces holding at least one summary whose embedding is missing or stale.
///
/// The scheduler is instance-wide; this queue, like the summary queue, is
/// per-tenant. Same set-difference shape as [`summaries_needing_embedding`],
/// collapsed to the workspace, and ordered so a bounded pass cannot serve the
/// same few workspaces forever while the rest starve.
pub async fn workspaces_with_summaries_needing_embedding(
    pool: &sqlx::PgPool,
    embed_model: &str,
    summariser_model: &str,
    limit: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT c.workspace_id
         FROM communities c
         JOIN community_summaries s ON s.community_id = c.id AND s.model_id = $2
         LEFT JOIN community_summary_embeddings e
           ON e.community_id = c.id AND e.model_id = $1
         WHERE e.community_id IS NULL OR e.summary_hash IS DISTINCT FROM md5(s.summary)
         GROUP BY c.workspace_id
         ORDER BY min(c.created_at)
         LIMIT $3",
    )
    .bind(embed_model)
    .bind(summariser_model)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Summaries whose embedding is missing or stale, for the worker to redo.
///
/// A set-difference QUERY, like every other queue here: no status column, no
/// claim state, and it re-evaluates on every poll so a crash mid-pass costs
/// nothing.
pub async fn summaries_needing_embedding(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    embed_model: &str,
    summariser_model: &str,
    limit: i64,
) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT c.id, s.summary
         FROM communities c
         JOIN community_summaries s ON s.community_id = c.id AND s.model_id = $3
         LEFT JOIN community_summary_embeddings e
           ON e.community_id = c.id AND e.model_id = $2
         WHERE c.workspace_id = $1
           AND (e.community_id IS NULL OR e.summary_hash IS DISTINCT FROM md5(s.summary))
         ORDER BY c.id
         LIMIT $4",
    )
    .bind(workspace_id)
    .bind(embed_model)
    .bind(summariser_model)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Select candidate communities by SEMANTIC similarity to the question.
///
/// This is what M6-3 replaces size-ranking with. A question about a niche topic
/// now reaches the theme that is ABOUT it, rather than the workspace's biggest
/// themes — and it reaches it even when the summary uses entirely different
/// words, which is the whole point of an embedding and the thing term overlap
/// could never do.
///
/// Communities whose summary is not yet embedded are NOT silently dropped:
/// they come back through `summaries_for_search`'s skipped count, so an answer
/// still reports what it could not consult.
pub async fn summaries_by_similarity(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    summariser_model: &str,
    embed_model: &str,
    question_vec: &[f32],
    limit: i64,
) -> Result<SummarySelection, sqlx::Error> {
    let limit = limit.clamp(1, 50);

    let candidates: Vec<SummaryCandidate> = sqlx::query_as::<_, (Uuid, String, String, i64)>(
        "SELECT c.id, s.summary, s.state, count(cm.entity_id) AS member_count
         FROM communities c
         JOIN community_summaries s ON s.community_id = c.id AND s.model_id = $2
         JOIN community_summary_embeddings e
           ON e.community_id = c.id AND e.model_id = $3
         LEFT JOIN community_members cm ON cm.community_id = c.id
         WHERE c.workspace_id = $1
         GROUP BY c.id, s.summary, s.state, e.embedding
         ORDER BY e.embedding <=> $4, c.id
         LIMIT $5",
    )
    .bind(workspace_id)
    .bind(summariser_model)
    .bind(embed_model)
    .bind(pgvector::Vector::from(question_vec.to_vec()))
    .bind(limit)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(community_id, summary, state, member_count)| SummaryCandidate {
        community_id,
        summary,
        state,
        member_count,
    })
    .collect();

    // Anything without a usable summary OR without an embedding is unconsulted,
    // and says so rather than vanishing.
    let skipped_unsummarised: i64 = sqlx::query_scalar(
        "SELECT count(*)
         FROM communities c
         WHERE c.workspace_id = $1
           AND NOT EXISTS (
               SELECT 1 FROM community_summaries s
               JOIN community_summary_embeddings e
                 ON e.community_id = s.community_id AND e.model_id = $3
               WHERE s.community_id = c.id AND s.model_id = $2
           )",
    )
    .bind(workspace_id)
    .bind(summariser_model)
    .bind(embed_model)
    .fetch_one(pool)
    .await?;

    Ok(SummarySelection {
        candidates,
        skipped_unsummarised,
    })
}

/// How many themes have a CURRENT summary, over how many could have one.
///
/// "Could have one" means the community has members — an empty community has
/// nothing to describe and is never queued, so counting it would show a
/// backlog that never reaches zero. "Current" means the stored summary still
/// matches the community's membership and was written by this model; a stale
/// one is pending work, not done work.
///
/// Same set-difference shape as the queue queries above, counted rather than
/// listed, so the number a UI shows and the work the scheduler picks up can
/// never disagree.
pub async fn summary_progress(
    pool: &sqlx::PgPool,
    summariser_model: &str,
    workspace_id: Option<Uuid>,
) -> Result<(i64, i64), sqlx::Error> {
    sqlx::query_as(
        "SELECT
             count(*) FILTER (
               WHERE s.community_id IS NOT NULL
                 AND s.model_id = $1
                 AND s.member_set_hash IS NOT DISTINCT FROM c.member_set_hash
             ),
             count(*)
         FROM communities c
         LEFT JOIN community_summaries s ON s.community_id = c.id
         WHERE ($2::uuid IS NULL OR c.workspace_id = $2)
           AND EXISTS (SELECT 1 FROM community_members m WHERE m.community_id = c.id)",
    )
    .bind(summariser_model)
    .bind(workspace_id)
    .fetch_one(pool)
    .await
}

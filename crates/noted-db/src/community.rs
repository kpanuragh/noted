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

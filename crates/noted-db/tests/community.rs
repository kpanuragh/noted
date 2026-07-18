use noted_db::community;
use uuid::Uuid;

async fn setup() -> (noted_db::PgPool, Uuid) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    let ws: Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('community-test') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    (pool, ws)
}

/// `n` entities in one workspace, named uniquely so tests sharing the dev
/// database never collide on `entities`' `UNIQUE (workspace_id, name)`.
async fn entities(pool: &noted_db::PgPool, ws: Uuid, n: usize) -> Vec<Uuid> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        ids.push(
            noted_db::graph::resolve_entity(
                pool,
                ws,
                &format!("e{i}-{}", Uuid::new_v4()),
                Some("CONCEPT"),
                None,
            )
            .await
            .unwrap(),
        );
    }
    ids
}

/// The workspace's stored partition in CANONICAL FORM: member sets, each
/// sorted, the whole list sorted. Never a raw label vector — two partitions can
/// be identical as set-partitions while carrying different community ids, so
/// comparing ids (or their order) reports spurious inequality. Same discipline
/// the spec §7 requires of the Louvain tests.
async fn stored_partition(pool: &noted_db::PgPool, ws: Uuid) -> Vec<Vec<Uuid>> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT c.id, cm.entity_id
         FROM communities c
         JOIN community_members cm ON cm.community_id = c.id
         WHERE c.workspace_id = $1",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap();

    let mut by_community: std::collections::BTreeMap<Uuid, Vec<Uuid>> = Default::default();
    for (cid, eid) in rows {
        by_community.entry(cid).or_default().push(eid);
    }
    let mut out: Vec<Vec<Uuid>> = by_community
        .into_values()
        .map(|mut m| {
            m.sort();
            m
        })
        .collect();
    out.sort();
    out
}

fn canonical(partition: &[Vec<Uuid>]) -> Vec<Vec<Uuid>> {
    let mut out: Vec<Vec<Uuid>> = partition
        .iter()
        .map(|m| {
            let mut m = m.clone();
            m.sort();
            m
        })
        .collect();
    out.sort();
    out
}

/// ATOMICITY — the property the whole cold path rests on.
///
/// `swap_partition` replaces a workspace's entire partition, which means it
/// must DELETE the old communities before the new ones exist. If that delete
/// and the inserts are not one transaction, a failure between them leaves the
/// workspace with NO partition at all — the graph unqueryable until the next
/// cold run, which is exactly the outage the hot/cold design exists to avoid.
///
/// A swap that SUCCEEDS proves nothing about this, so the failure is FORCED:
/// the second community of the incoming partition names an entity id that does
/// not exist, so `community_members`' foreign key aborts the insert — after the
/// delete and after the first community has already been written. Rollback is
/// the only thing that can save the previous partition, and the assertions
/// below check the previous partition specifically, not merely that something
/// is present.
#[tokio::test]
async fn a_swap_that_fails_midway_leaves_the_previous_partition_intact() {
    let (pool, ws) = setup().await;
    let e = entities(&pool, ws, 4).await;

    let before = vec![vec![e[0], e[1]], vec![e[2], e[3]]];
    community::swap_partition(&pool, ws, &[(0, before[0].clone()), (0, before[1].clone())])
        .await
        .unwrap();
    let snapshot = stored_partition(&pool, ws).await;
    assert_eq!(
        snapshot,
        canonical(&before),
        "sanity: the first swap must land before we can say anything about surviving a second"
    );

    // The doomed swap. Community one is writable; community two references a
    // non-existent entity and violates community_members' FK.
    let ghost = Uuid::new_v4();
    let doomed = vec![(0, vec![e[0], e[2]]), (0, vec![e[1], e[3], ghost])];
    let err = community::swap_partition(&pool, ws, &doomed)
        .await
        .expect_err("a partition naming an entity that does not exist must fail, not half-apply");
    assert!(
        format!("{err}").contains("foreign key") || format!("{err:?}").contains("foreign key"),
        "sanity: the failure must be the FK we forced, not something incidental: {err:?}"
    );

    assert_eq!(
        stored_partition(&pool, ws).await,
        canonical(&before),
        "the PREVIOUS partition must survive a failed swap byte for byte — not be emptied, \
         not be left holding the first half of the doomed one"
    );

    let orphaned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM community_members cm
         JOIN communities c ON c.id = cm.community_id
         WHERE c.workspace_id = $1 AND cm.entity_id = $2",
    )
    .bind(ws)
    .bind(ghost)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(orphaned, 0, "no fragment of the failed swap may remain");
}

/// TENANCY. Communities are per-workspace by construction, so a cold run for
/// workspace A must not touch B's partition — not its rows, not its community
/// ids (which B's summaries are keyed by). This is M2a's lesson applied before
/// the fact: every DELETE in `swap_partition` carries `workspace_id`.
#[tokio::test]
async fn swapping_one_workspaces_partition_cannot_alter_anothers() {
    let (pool, ws_a) = setup().await;
    let (_, ws_b) = setup().await;
    let a = entities(&pool, ws_a, 2).await;
    let b = entities(&pool, ws_b, 2).await;

    community::swap_partition(&pool, ws_a, &[(0, vec![a[0], a[1]])])
        .await
        .unwrap();
    community::swap_partition(&pool, ws_b, &[(0, vec![b[0]]), (0, vec![b[1]])])
        .await
        .unwrap();
    let b_before: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM communities WHERE workspace_id = $1 ORDER BY id")
            .bind(ws_b)
            .fetch_all(&pool)
            .await
            .unwrap();

    // Re-cluster A completely: same entities, different partition.
    community::swap_partition(&pool, ws_a, &[(0, vec![a[0]]), (0, vec![a[1]])])
        .await
        .unwrap();

    assert_eq!(
        stored_partition(&pool, ws_b).await,
        canonical(&[vec![b[0]], vec![b[1]]]),
        "workspace B's membership must be untouched by A's swap"
    );
    let b_after: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM communities WHERE workspace_id = $1 ORDER BY id")
            .bind(ws_b)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        b_before, b_after,
        "B's community IDS must survive too — B's summaries are keyed by them, so \
         deleting and re-creating B's rows would silently discard every summary it holds"
    );
}

/// `member_set_hash` sorts, and sorting is the entire point.
///
/// The hash is what decides whether a community's summary survives a swap. A
/// cold run that re-derives the same communities may well emit their members in
/// a different order, so an order-sensitive hash would report every community
/// changed and trigger a full summary regeneration storm on a partition that
/// did not actually move.
#[tokio::test]
async fn member_set_hash_ignores_member_order() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();

    assert_eq!(
        community::member_set_hash(&[a, b, c]),
        community::member_set_hash(&[c, a, b]),
        "the same members in a different order are the same community"
    );
    assert_eq!(
        community::member_set_hash(&[a, b, c]),
        community::member_set_hash(&[b, c, a]),
    );
}

/// A PURE RELABEL — the same set-partition with different community ids — must
/// leave every hash identical.
///
/// This is the property M2b research measured: raw label vectors compare
/// unequal after a relabel while member-set hashes compare equal, so zero
/// summaries regenerate. Communities here are described only by their members,
/// and the hash never sees an id, which is what makes the relabel invisible.
#[tokio::test]
async fn member_set_hash_ignores_a_pure_community_relabel() {
    let e: Vec<Uuid> = (0..6).map(|_| Uuid::new_v4()).collect();

    // Same set-partition, communities enumerated in a different order and with
    // their members in a different order — i.e. relabelled.
    let run_one = [vec![e[0], e[1], e[2]], vec![e[3], e[4]], vec![e[5]]];
    let run_two = [vec![e[5]], vec![e[4], e[3]], vec![e[2], e[0], e[1]]];

    let mut h1: Vec<String> = run_one
        .iter()
        .map(|m| community::member_set_hash(m))
        .collect();
    let mut h2: Vec<String> = run_two
        .iter()
        .map(|m| community::member_set_hash(m))
        .collect();
    h1.sort();
    h2.sort();
    assert_eq!(
        h1, h2,
        "a relabel must produce the identical multiset of member-set hashes; if it does not, \
         every partition swap regenerates every summary"
    );
}

/// The other direction, without which the test above is satisfied by a constant
/// function: genuinely different membership must hash differently.
#[tokio::test]
async fn member_set_hash_differs_for_different_membership() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();

    assert_ne!(
        community::member_set_hash(&[a, b]),
        community::member_set_hash(&[a, c]),
        "swapping one member for another is a different community"
    );
    assert_ne!(
        community::member_set_hash(&[a, b]),
        community::member_set_hash(&[a, b, c]),
        "adding a member is a different community"
    );
    assert_ne!(
        community::member_set_hash(&[a, b]),
        community::member_set_hash(&[a]),
        "removing a member is a different community"
    );
}

/// THE HASH ONLY PAYS OFF IF THE SWAP HONOURS IT.
///
/// `community_summaries` is keyed by `community_id` and cascades on delete, so
/// a swap implemented as "delete every community, insert the new ones" would
/// destroy every summary on every cold run, and M2b-3's "unchanged membership
/// ⇒ zero summariser calls" would be unreachable no matter how good the hash
/// is. `swap_partition` therefore MATCHES incoming communities to existing ones
/// by `(level, member_set_hash)` and preserves the row — id and all — when they
/// agree. This test pins that: a relabelled-but-identical partition keeps its
/// summaries; a genuinely changed community loses its own and only its own.
#[tokio::test]
async fn a_swap_preserves_the_summaries_of_communities_whose_membership_is_unchanged() {
    let (pool, ws) = setup().await;
    let e = entities(&pool, ws, 4).await;

    let kept = vec![e[0], e[1]];
    let moved = vec![e[2], e[3]];
    community::swap_partition(&pool, ws, &[(0, kept.clone()), (0, moved.clone())])
        .await
        .unwrap();

    for members in [&kept, &moved] {
        let hash = community::member_set_hash(members);
        sqlx::query(
            "INSERT INTO community_summaries (community_id, model_id, summary, state, member_set_hash)
             SELECT id, 'stub', 'a summary', 'valid', $2
             FROM communities WHERE workspace_id = $1 AND member_set_hash = $2",
        )
        .bind(ws)
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();
    }

    // A new cold run: `kept` is re-derived identically (members in a different
    // order, community enumerated second — a pure relabel), `moved` splits.
    community::swap_partition(
        &pool,
        ws,
        &[(0, vec![e[2]]), (0, vec![e[3]]), (0, vec![e[1], e[0]])],
    )
    .await
    .unwrap();

    let survived: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM community_summaries s
         JOIN communities c ON c.id = s.community_id
         WHERE c.workspace_id = $1 AND c.member_set_hash = $2",
    )
    .bind(ws)
    .bind(community::member_set_hash(&kept))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        survived, 1,
        "an unchanged community must keep its row and therefore its summary; regenerating it \
         is a model call spent to reproduce a summary that was already correct"
    );

    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM community_summaries s
         JOIN communities c ON c.id = s.community_id
         WHERE c.workspace_id = $1",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        total, 1,
        "and the split community's summary must be gone — it described a membership that no \
         longer exists, so keeping it would serve prose about the wrong entities"
    );
}

/// A swap must leave nothing behind. Communities absent from the new partition
/// are deleted, and their members with them; otherwise the partition
/// accumulates every community the workspace has ever had and stops being a
/// partition at all.
#[tokio::test]
async fn a_swap_removes_communities_the_new_partition_does_not_contain() {
    let (pool, ws) = setup().await;
    let e = entities(&pool, ws, 3).await;

    community::swap_partition(
        &pool,
        ws,
        &[(0, vec![e[0]]), (0, vec![e[1]]), (0, vec![e[2]])],
    )
    .await
    .unwrap();
    community::swap_partition(&pool, ws, &[(0, vec![e[0], e[1], e[2]])])
        .await
        .unwrap();

    assert_eq!(
        stored_partition(&pool, ws).await,
        canonical(&[vec![e[0], e[1], e[2]]]),
        "the workspace must hold exactly the new partition, not the union of both"
    );

    // An empty partition is legitimate (a workspace with no clusterable graph)
    // and must clear the table rather than being ignored as a no-op.
    community::swap_partition(&pool, ws, &[]).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM communities WHERE workspace_id = $1")
        .bind(ws)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "an empty partition must clear the workspace");
}

/// The churn counter driving the cold path. A workspace that has never been
/// clustered has no row, and must read as (0, None) rather than erroring —
/// the cold-path scheduler asks about every workspace, including brand-new
/// ones.
#[tokio::test]
async fn churn_reads_zero_before_any_edge_changes_and_accumulates_after() {
    let (pool, ws) = setup().await;

    assert_eq!(
        community::churn(&pool, ws).await.unwrap(),
        (0, None),
        "a workspace with no churn row has changed no edges and has never had a full run"
    );

    community::bump_churn(&pool, ws, 3).await.unwrap();
    community::bump_churn(&pool, ws, 4).await.unwrap();
    let (changed, last_run) = community::churn(&pool, ws).await.unwrap();
    assert_eq!(
        changed, 7,
        "bumps must ACCUMULATE — an overwriting counter would lose every edge change but the \
         last and the cold path would never trip its threshold"
    );
    assert_eq!(
        last_run, None,
        "counting churn is not a full run; only the cold path may stamp that"
    );
}

/// Churn is per-workspace: one tenant's edit storm must not drag another
/// tenant's graph into a full re-clustering.
#[tokio::test]
async fn churn_is_scoped_to_its_workspace() {
    let (pool, ws_a) = setup().await;
    let (_, ws_b) = setup().await;

    community::bump_churn(&pool, ws_a, 100).await.unwrap();

    assert_eq!(community::churn(&pool, ws_a).await.unwrap().0, 100);
    assert_eq!(
        community::churn(&pool, ws_b).await.unwrap().0,
        0,
        "workspace B's churn must be unaffected by A's"
    );
}

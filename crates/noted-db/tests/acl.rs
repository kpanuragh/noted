//! M4-3 — per-page permissions.
use noted_db::acl;
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&pool).await.unwrap();
    pool
}

async fn workspace(pool: &noted_db::PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('acl-test') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn user(pool: &noted_db::PgPool) -> Uuid {
    let email = format!("acl{}@example.com", Uuid::new_v4().simple());
    noted_db::users::create(pool, &email, "hash", "ACL")
        .await
        .unwrap()
        .id
}

async fn page(pool: &noted_db::PgPool, ws: Uuid, parent: Option<Uuid>, title: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO pages (workspace_id, parent_id, title) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(ws)
    .bind(parent)
    .bind(title)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// With no override anywhere, a workspace member reads everything. Absence of a
/// row must not mean absence of access, or adding ACLs would silently lock
/// every existing workspace out of its own pages.
#[tokio::test]
async fn with_no_overrides_a_member_reads_every_page() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let u = user(&pool).await;
    let root = page(&pool, ws, None, "Root").await;
    let child = page(&pool, ws, Some(root), "Child").await;

    let readable = acl::readable_pages(&pool, ws, u).await.unwrap();
    assert!(readable.contains(&root));
    assert!(readable.contains(&child));
    assert!(acl::can_read(&pool, child, u).await.unwrap());
}

/// **The headline property: a denial hides the page AND its descendants.**
///
/// MECHANISM PROTECTED: the recursive inheritance in `readable_pages_cte!`.
/// Flatten it to a per-page lookup and the grandchild leaks.
#[tokio::test]
async fn denying_a_page_hides_its_whole_subtree() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let u = user(&pool).await;

    let root = page(&pool, ws, None, "Root").await;
    let private = page(&pool, ws, Some(root), "Salaries").await;
    let under_private = page(&pool, ws, Some(private), "Q3 review").await;
    let deeper = page(&pool, ws, Some(under_private), "Notes").await;
    let sibling = page(&pool, ws, Some(root), "Lunch menu").await;

    acl::set_access(&pool, private, u, "none").await.unwrap();

    let readable = acl::readable_pages(&pool, ws, u).await.unwrap();
    assert!(readable.contains(&root), "the root is untouched");
    assert!(readable.contains(&sibling), "a sibling is untouched");
    assert!(!readable.contains(&private), "the denied page is hidden");
    assert!(
        !readable.contains(&under_private),
        "and so is its child — inheritance is the point"
    );
    assert!(
        !readable.contains(&deeper),
        "and its grandchild; depth must not rescue it"
    );
}

/// A grant BELOW a denial wins, because the nearest override decides.
///
/// This is what makes the rule an override rather than a blanket: "you cannot
/// see the HR subtree, except this one page in it".
#[tokio::test]
async fn a_grant_inside_a_denied_subtree_wins() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let u = user(&pool).await;

    let root = page(&pool, ws, None, "Root").await;
    let hr = page(&pool, ws, Some(root), "HR").await;
    let shared = page(&pool, ws, Some(hr), "Handbook").await;
    let under_shared = page(&pool, ws, Some(shared), "Appendix").await;

    acl::set_access(&pool, hr, u, "none").await.unwrap();
    acl::set_access(&pool, shared, u, "read").await.unwrap();

    let readable = acl::readable_pages(&pool, ws, u).await.unwrap();
    assert!(!readable.contains(&hr));
    assert!(readable.contains(&shared), "the nearer grant must win");
    assert!(
        readable.contains(&under_shared),
        "and its descendants inherit the grant, not the ancestor's denial"
    );
}

/// Clearing an override restores inheritance rather than granting access.
#[tokio::test]
async fn clearing_an_override_restores_inheritance() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let u = user(&pool).await;

    let root = page(&pool, ws, None, "Root").await;
    let mid = page(&pool, ws, Some(root), "Mid").await;
    let leaf = page(&pool, ws, Some(mid), "Leaf").await;

    acl::set_access(&pool, root, u, "none").await.unwrap();
    acl::set_access(&pool, mid, u, "read").await.unwrap();
    assert!(acl::can_read(&pool, leaf, u).await.unwrap());

    // Removing the grant on `mid` must make `leaf` fall back to root's DENIAL,
    // not to the default.
    acl::clear_access(&pool, mid, u).await.unwrap();
    assert!(
        !acl::can_read(&pool, leaf, u).await.unwrap(),
        "clearing a grant restores inheritance from the denied ancestor"
    );
}

/// The bulk rule and the single-page rule must agree, on every page.
///
/// Two implementations of one rule is the exact shape of this project's worst
/// bugs — a permission that means one thing in the search query and another in
/// the page fetch is a leak. This test compares them exhaustively over a tree
/// with grants, denials and a re-grant.
#[tokio::test]
async fn the_bulk_and_single_page_checks_agree() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let u = user(&pool).await;

    let root = page(&pool, ws, None, "Root").await;
    let a = page(&pool, ws, Some(root), "A").await;
    let a1 = page(&pool, ws, Some(a), "A1").await;
    let a2 = page(&pool, ws, Some(a1), "A2").await;
    let b = page(&pool, ws, Some(root), "B").await;

    acl::set_access(&pool, a, u, "none").await.unwrap();
    acl::set_access(&pool, a1, u, "read").await.unwrap();

    let bulk = acl::readable_pages(&pool, ws, u).await.unwrap();
    for p in [root, a, a1, a2, b] {
        let single = acl::can_read(&pool, p, u).await.unwrap();
        assert_eq!(
            bulk.contains(&p),
            single,
            "the two implementations disagree about page {p}"
        );
    }
}

/// One user's denial is not another's.
#[tokio::test]
async fn an_acl_applies_only_to_the_user_it_names() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let denied = user(&pool).await;
    let other = user(&pool).await;

    let root = page(&pool, ws, None, "Root").await;
    let secret = page(&pool, ws, Some(root), "Secret").await;
    acl::set_access(&pool, secret, denied, "none").await.unwrap();

    assert!(!acl::can_read(&pool, secret, denied).await.unwrap());
    assert!(
        acl::can_read(&pool, secret, other).await.unwrap(),
        "another member must be unaffected"
    );
}

// ------------------------------------------------- retrieval surfaces (M4-3b) --

/// A page with a block (so FTS sees it), a chunk, and an embedding.
async fn searchable_page(
    pool: &noted_db::PgPool,
    ws: Uuid,
    parent: Option<Uuid>,
    title: &str,
    text: &str,
    model: &str,
    axis: usize,
) -> (Uuid, String) {
    let id = page(pool, ws, parent, title).await;
    sqlx::query(
        "INSERT INTO blocks (page_id, block_index, node_type, text, content_hash)
         VALUES ($1, 0, 'paragraph', $2, md5($2))",
    )
    .bind(id)
    .bind(text)
    .execute(pool)
    .await
    .unwrap();
    let hash = format!("aclq-{}", Uuid::new_v4());
    noted_db::chunks::upsert(pool, &[(hash.clone(), text.to_string(), 10)])
        .await
        .unwrap();
    noted_db::chunks::set_page_chunks(pool, id, &[hash.clone()])
        .await
        .unwrap();
    let mut v = vec![0.0f32; 768];
    v[axis] = 1.0;
    noted_db::chunks::store_embedding(pool, &hash, model, &v)
        .await
        .unwrap();
    (id, hash)
}

/// **A denied page's CONTENT must not surface through hybrid search.**
///
/// Page-addressed routes were already covered by the `MemberPage` extractor;
/// this is the leak that stayed open — search returns *content*, so a user who
/// cannot open a page could still read it a snippet at a time.
///
/// MECHANISM PROTECTED: the `readable_pages` joins in `hybrid`'s lexical arm and
/// in the vector arm's `EXISTS`. Remove either and the denied page comes back.
#[tokio::test]
async fn a_denied_page_does_not_surface_through_hybrid_search() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let allowed_user = user(&pool).await;
    let denied_user = user(&pool).await;
    let model = format!("acl-{}", Uuid::new_v4());

    let root = page(&pool, ws, None, "Root").await;
    let (secret, _) = searchable_page(
        &pool,
        ws,
        Some(root),
        "Salary review",
        "the confidential compensation band for staff engineers",
        &model,
        11,
    )
    .await;

    let mut q_vec = vec![0.0f32; 768];
    q_vec[11] = 1.0;
    let q = "confidential compensation band";

    // Premise: a user with no denial DOES find it. Without this the assertion
    // below could pass because the fixture was never searchable at all.
    let allowed = noted_db::search::hybrid(&pool, ws, allowed_user, q, &q_vec, &model, 10)
        .await
        .unwrap();
    assert!(
        allowed.iter().any(|h| h.page_id == secret),
        "premise: the page must be findable by someone who may read it"
    );

    acl::set_access(&pool, secret, denied_user, "none")
        .await
        .unwrap();

    let denied = noted_db::search::hybrid(&pool, ws, denied_user, q, &q_vec, &model, 10)
        .await
        .unwrap();
    assert!(
        !denied.iter().any(|h| h.page_id == secret),
        "a denied page's content must not surface through search"
    );
}

/// The same, for quick find — which searches TITLES, so a denial has to hide the
/// title too.
#[tokio::test]
async fn a_denied_page_does_not_surface_through_quick_find() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let allowed_user = user(&pool).await;
    let denied_user = user(&pool).await;

    let root = page(&pool, ws, None, "Root").await;
    let secret = page(&pool, ws, Some(root), "Acquisition terms").await;

    let allowed = noted_db::search::quick_find(&pool, ws, allowed_user, "Acquisition", 10)
        .await
        .unwrap();
    assert!(
        allowed.iter().any(|h| h.page_id == secret),
        "premise: findable by someone who may read it"
    );

    acl::set_access(&pool, secret, denied_user, "none")
        .await
        .unwrap();

    let denied = noted_db::search::quick_find(&pool, ws, denied_user, "Acquisition", 10)
        .await
        .unwrap();
    assert!(
        !denied.iter().any(|h| h.page_id == secret),
        "even the TITLE must not leak"
    );
}

/// **The subtle one: a denied page must not arrive as a GRAPH HOP.**
///
/// Local search reaches chunks through `page_chunks` after traversing edges, so
/// a page that search itself would never return could still be pulled in
/// because something readable is connected to it. That is the leak the graph
/// creates and plain search does not.
#[tokio::test]
async fn a_denied_page_does_not_arrive_as_a_graph_hop() {
    let pool = pool().await;
    let ws = workspace(&pool).await;
    let denied_user = user(&pool).await;
    let model = format!("acl-{}", Uuid::new_v4());

    let root = page(&pool, ws, None, "Root").await;
    let (public, public_hash) = searchable_page(
        &pool,
        ws,
        Some(root),
        "Public",
        "the quarterly planning meeting covered headcount",
        &model,
        21,
    )
    .await;
    let (secret, secret_hash) = searchable_page(
        &pool,
        ws,
        Some(root),
        "Secret",
        "sourdough starter needs feeding twice daily",
        &model,
        400,
    )
    .await;

    // One edge chains the two chunks through a shared entity, so the secret is
    // exactly one hop from the public page.
    let a = noted_db::graph::resolve_entity(&pool, ws, &format!("a-{}", Uuid::new_v4()), Some("CONCEPT"), None).await.unwrap();
    let b = noted_db::graph::resolve_entity(&pool, ws, &format!("b-{}", Uuid::new_v4()), Some("CONCEPT"), None).await.unwrap();
    let c = noted_db::graph::resolve_entity(&pool, ws, &format!("c-{}", Uuid::new_v4()), Some("CONCEPT"), None).await.unwrap();
    noted_db::graph::replace_chunk_edges(&pool, ws, &public_hash, &model, &[(a, b, "rel".into(), 1.0)]).await.unwrap();
    noted_db::graph::replace_chunk_edges(&pool, ws, &secret_hash, &model, &[(b, c, "rel".into(), 1.0)]).await.unwrap();

    let seeds = vec![noted_db::graph_search::SeedChunk {
        content_hash: public_hash.clone(),
        rank: 1,
    }];

    // Premise: the hop DOES reach the secret for a user with no denial.
    let open_user = user(&pool).await;
    let reachable = noted_db::graph_search::local_search_chunks(&pool, ws, open_user, &seeds, 20)
        .await
        .unwrap();
    assert!(
        reachable.iter().any(|h| h.page_id == secret),
        "premise: the graph must actually reach the secret page, or this proves nothing"
    );
    assert!(reachable.iter().any(|h| h.page_id == public));

    acl::set_access(&pool, secret, denied_user, "none")
        .await
        .unwrap();

    let hits = noted_db::graph_search::local_search_chunks(&pool, ws, denied_user, &seeds, 20)
        .await
        .unwrap();
    assert!(
        !hits.iter().any(|h| h.page_id == secret),
        "a denied page must not be reachable as a graph hop"
    );
    assert!(
        hits.iter().any(|h| h.page_id == public),
        "and the readable seed must still come back"
    );
}

//! M5-1 — comments and mentions.
use noted_db::comments;
use uuid::Uuid;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let p = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&p).await.unwrap();
    p
}
async fn fixture(p: &noted_db::PgPool) -> (Uuid, Uuid, Uuid) {
    let w: Uuid = sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ('cmt') RETURNING id")
        .fetch_one(p).await.unwrap();
    let page: Uuid = sqlx::query_scalar("INSERT INTO pages (workspace_id, title) VALUES ($1,'P') RETURNING id")
        .bind(w).fetch_one(p).await.unwrap();
    let email = format!("c{}@example.com", Uuid::new_v4().simple());
    let user = noted_db::users::create(p, &email, "h", "C").await.unwrap().id;
    (w, page, user)
}

#[tokio::test]
async fn a_comment_round_trips_with_its_anchor() {
    let pool = pool().await;
    let (_w, page, user) = fixture(&pool).await;

    let c = comments::create(&pool, page, user, None, "needs a citation",
        Some((0, vec![1, 2, 3], "quick brown".into()))).await.unwrap();

    let all = comments::for_page(&pool, page).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, c.id);
    assert_eq!(all[0].anchor.as_deref(), Some(&[1u8, 2, 3][..]));
    assert_eq!(all[0].quote.as_deref(), Some("quick brown"));
}

/// A page-level comment has no anchor — a legitimate state, not a missing one.
#[tokio::test]
async fn a_page_level_comment_needs_no_anchor() {
    let pool = pool().await;
    let (_w, page, user) = fixture(&pool).await;
    let c = comments::create(&pool, page, user, None, "overall: good", None).await.unwrap();
    assert!(c.anchor.is_none() && c.quote.is_none() && c.block_index.is_none());
}

/// Replies belong to their parent, and resolving a thread resolves the replies
/// with it — a half-resolved thread is a UI nobody can read.
#[tokio::test]
async fn resolving_a_thread_resolves_its_replies() {
    let pool = pool().await;
    let (_w, page, user) = fixture(&pool).await;
    let root = comments::create(&pool, page, user, None, "question?", None).await.unwrap();
    comments::create(&pool, page, user, Some(root.id), "answer", None).await.unwrap();

    comments::set_resolved(&pool, root.id, true).await.unwrap();
    let all = comments::for_page(&pool, page).await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|c| c.resolved), "the whole thread must resolve together");
}

/// Deleting a page takes its comments — by CASCADE.
#[tokio::test]
async fn deleting_a_page_takes_its_comments() {
    let pool = pool().await;
    let (_w, page, user) = fixture(&pool).await;
    comments::create(&pool, page, user, None, "text", None).await.unwrap();

    sqlx::query("DELETE FROM pages WHERE id = $1").bind(page).execute(&pool).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM comments WHERE page_id = $1")
        .bind(page).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
}

/// **Mentions are STORED, not re-parsed on read.**
///
/// A mention is a fact — "this comment notifies Alice". Deriving it from prose
/// on every read means the notification list silently changes if the parser
/// changes, and a notification someone already acted on can vanish.
#[tokio::test]
async fn a_mention_notifies_the_user_it_names() {
    let pool = pool().await;
    let (_w, page, author) = fixture(&pool).await;
    let mentioned = noted_db::users::create(
        &pool, &format!("m{}@example.com", Uuid::new_v4().simple()), "h", "M")
        .await.unwrap().id;

    let c = comments::create(&pool, page, author, None, "@M please look", None).await.unwrap();
    comments::add_mentions(&pool, c.id, &[mentioned], &[]).await.unwrap();

    let mine = comments::mentioning(&pool, mentioned, 50).await.unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].id, c.id);

    // And the author, who was not mentioned, has nothing.
    assert!(comments::mentioning(&pool, author, 50).await.unwrap().is_empty());
}

/// A mention on an ARCHIVED page does not appear in a notification list — the
/// page is deleted as far as the user is concerned.
#[tokio::test]
async fn a_mention_on_an_archived_page_is_not_notified() {
    let pool = pool().await;
    let (_w, page, author) = fixture(&pool).await;
    let mentioned = noted_db::users::create(
        &pool, &format!("m{}@example.com", Uuid::new_v4().simple()), "h", "M")
        .await.unwrap().id;
    let c = comments::create(&pool, page, author, None, "@M", None).await.unwrap();
    comments::add_mentions(&pool, c.id, &[mentioned], &[]).await.unwrap();
    assert_eq!(comments::mentioning(&pool, mentioned, 50).await.unwrap().len(), 1, "premise");

    sqlx::query("UPDATE pages SET archived_at = now() WHERE id = $1")
        .bind(page).execute(&pool).await.unwrap();

    assert!(comments::mentioning(&pool, mentioned, 50).await.unwrap().is_empty());
}

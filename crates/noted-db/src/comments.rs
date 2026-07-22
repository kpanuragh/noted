//! Threaded comments, anchored to CRDT positions.
use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, serde::Serialize, sqlx::FromRow)]
pub struct Comment {
    pub id: Uuid,
    pub page_id: Uuid,
    pub author_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub body: String,
    pub block_index: Option<i32>,
    pub anchor: Option<Vec<u8>>,
    pub quote: Option<String>,
    pub resolved: bool,
    pub created_at: DateTime<Utc>,
}

/// Post a comment. `anchor` is `None` for a page-level comment.
pub async fn create(
    pool: &sqlx::PgPool,
    page_id: Uuid,
    author_id: Uuid,
    parent_id: Option<Uuid>,
    body: &str,
    anchor: Option<(i32, Vec<u8>, String)>,
) -> Result<Comment, sqlx::Error> {
    let (block_index, encoded, quote) = match anchor {
        Some((b, e, q)) => (Some(b), Some(e), Some(q)),
        None => (None, None, None),
    };

    sqlx::query_as::<_, Comment>(
        "INSERT INTO comments (page_id, author_id, parent_id, body, block_index, anchor, quote)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id, page_id, author_id, parent_id, body, block_index, anchor, quote,
                   resolved, created_at",
    )
    .bind(page_id)
    .bind(author_id)
    .bind(parent_id)
    .bind(body)
    .bind(block_index)
    .bind(encoded)
    .bind(quote)
    .fetch_one(pool)
    .await
}

/// Every comment on a page, oldest first — parents before their replies,
/// because `created_at` orders a one-level thread correctly by construction.
pub async fn for_page(pool: &sqlx::PgPool, page_id: Uuid) -> Result<Vec<Comment>, sqlx::Error> {
    sqlx::query_as::<_, Comment>(
        "SELECT id, page_id, author_id, parent_id, body, block_index, anchor, quote,
                resolved, created_at
         FROM comments WHERE page_id = $1 ORDER BY created_at, id",
    )
    .bind(page_id)
    .fetch_all(pool)
    .await
}

/// Resolve or unresolve a thread.
pub async fn set_resolved(
    pool: &sqlx::PgPool,
    comment_id: Uuid,
    resolved: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE comments SET resolved = $2 WHERE id = $1 OR parent_id = $1")
        .bind(comment_id)
        .bind(resolved)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record who and what a comment mentions.
///
/// Stored rather than re-parsed on every read: a mention is a FACT ("this
/// comment notifies Alice"), and deriving it from prose each time means the
/// notification list silently changes if the parser changes.
pub async fn add_mentions(
    pool: &sqlx::PgPool,
    comment_id: Uuid,
    users: &[Uuid],
    pages: &[Uuid],
) -> Result<(), sqlx::Error> {
    for user_id in users {
        sqlx::query(
            "INSERT INTO comment_user_mentions (comment_id, user_id)
             VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(comment_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    }
    for page_id in pages {
        sqlx::query(
            "INSERT INTO comment_page_mentions (comment_id, page_id)
             VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(comment_id)
        .bind(page_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Comments that mention this user, newest first — their notification list.
pub async fn mentioning(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<Comment>, sqlx::Error> {
    sqlx::query_as::<_, Comment>(
        "SELECT c.id, c.page_id, c.author_id, c.parent_id, c.body, c.block_index,
                c.anchor, c.quote, c.resolved, c.created_at
         FROM comments c
         JOIN comment_user_mentions m ON m.comment_id = c.id
         JOIN pages p ON p.id = c.page_id AND p.archived_at IS NULL
         WHERE m.user_id = $1
         ORDER BY c.created_at DESC
         LIMIT $2",
    )
    .bind(user_id)
    .bind(limit.clamp(1, 200))
    .fetch_all(pool)
    .await
}

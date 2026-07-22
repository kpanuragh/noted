//! Workspaces and who belongs to them.
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    /// The caller's role in this workspace. Only meaningful in the context of
    /// the user it was fetched for, which is why it rides on the row rather
    /// than living in a separate lookup nobody would remember to make.
    pub role: String,
}

/// Create a workspace and make `owner` its first member, in ONE transaction.
///
/// Atomic deliberately: a workspace with no members is unreachable by anyone —
/// not even the person who just made it — and there is no UI anywhere that
/// could repair it. A crash between the two inserts would leak exactly that,
/// permanently.
pub async fn create(
    pool: &sqlx::PgPool,
    name: &str,
    owner: Uuid,
) -> Result<Workspace, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let id: Uuid = sqlx::query_scalar("INSERT INTO workspaces (name) VALUES ($1) RETURNING id")
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;

    sqlx::query("INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(id)
        .bind(owner)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(Workspace {
        id,
        name: name.to_string(),
        role: "owner".to_string(),
    })
}

/// Every workspace this user belongs to, oldest first so the list is stable
/// across page loads rather than reshuffling under the switcher.
pub async fn for_user(pool: &sqlx::PgPool, user_id: Uuid) -> Result<Vec<Workspace>, sqlx::Error> {
    sqlx::query_as::<_, Workspace>(
        "SELECT w.id, w.name, m.role
         FROM workspace_members m
         JOIN workspaces w ON w.id = m.workspace_id
         WHERE m.user_id = $1
         ORDER BY m.created_at, w.id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// Is this user a member of this workspace?
///
/// THE function behind every authorization decision in the product, so it is
/// deliberately the narrowest possible question: one index lookup on the
/// primary key, no joins, no role interpretation. Roles are read by callers
/// that care; this answers only "may they see it at all".
pub async fn is_member(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Add someone to a workspace. Idempotent — re-inviting an existing member
/// updates their role rather than failing, which is what an admin pressing the
/// button twice means.
pub async fn add_member(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
    role: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, $3)
         ON CONFLICT (workspace_id, user_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(workspace_id)
    .bind(user_id)
    .bind(role)
    .execute(pool)
    .await?;
    Ok(())
}

/// The workspace a page belongs to, if the page exists and is live.
///
/// Needed because several routes are addressed by PAGE id, not workspace id —
/// `/api/pages/{id}/related` and the sync socket among them — and the
/// membership question still has to be answered for them. Resolving the page's
/// workspace here keeps that decision in one place instead of letting each
/// handler invent its own.
pub async fn workspace_of_page(
    pool: &sqlx::PgPool,
    page_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT workspace_id FROM pages WHERE id = $1 AND archived_at IS NULL")
        .bind(page_id)
        .fetch_optional(pool)
        .await
}

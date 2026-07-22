//! API tokens: programmatic access, scoped per route.
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// A token's identity and what it may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenIdentity {
    pub user_id: Uuid,
    pub workspace_id: Uuid,
    pub scopes: Vec<String>,
}

impl TokenIdentity {
    /// Does this token carry `scope`?
    ///
    /// Exact match only — no prefix logic, no wildcards, no implication that
    /// `pages:write` includes `pages:read`. Hierarchies are where scope checks
    /// go wrong, because the rule lives in the checker rather than in the
    /// token, and a reader auditing the token cannot see what it really grants.
    /// A token that needs both carries both.
    pub fn allows(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

pub async fn create(
    pool: &sqlx::PgPool,
    token_hash: &str,
    user_id: Uuid,
    workspace_id: Uuid,
    name: &str,
    scopes: &[String],
    expires_at: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO api_tokens (token_hash, user_id, workspace_id, name, scopes, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(token_hash)
    .bind(user_id)
    .bind(workspace_id)
    .bind(name)
    .bind(scopes)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Resolve a token, if it is live.
///
/// Expiry is enforced HERE, in the query, not by a sweeper — the same rule
/// sessions and share links follow. `last_used_at` is stamped so an operator
/// can find tokens nobody is using and revoke them.
pub async fn resolve(
    pool: &sqlx::PgPool,
    token_hash: &str,
) -> Result<Option<TokenIdentity>, sqlx::Error> {
    let row: Option<(Uuid, Uuid, Vec<String>)> = sqlx::query_as(
        "UPDATE api_tokens
         SET last_used_at = now()
         WHERE token_hash = $1 AND (expires_at IS NULL OR expires_at > now())
         RETURNING user_id, workspace_id, scopes",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(user_id, workspace_id, scopes)| TokenIdentity {
        user_id,
        workspace_id,
        scopes,
    }))
}

pub async fn revoke(pool: &sqlx::PgPool, token_hash: &str) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM api_tokens WHERE token_hash = $1")
        .bind(token_hash)
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, sqlx::FromRow)]
pub struct TokenSummary {
    pub name: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// A user's tokens. Deliberately WITHOUT `token_hash`: a hash is not a token,
/// but handing one to a client is still handing out a credential-shaped secret
/// with no use case behind it.
pub async fn for_user(
    pool: &sqlx::PgPool,
    user_id: Uuid,
) -> Result<Vec<TokenSummary>, sqlx::Error> {
    sqlx::query_as::<_, TokenSummary>(
        "SELECT name, scopes, expires_at, last_used_at, created_at
         FROM api_tokens WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

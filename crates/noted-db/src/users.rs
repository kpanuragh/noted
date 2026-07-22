//! Users and sessions.
//!
//! Like every other module here this deals in primitives only — it stores a
//! password HASH and a token HASH and has no opinion about how either was
//! produced. Argon2 and token generation live in `noted-server`, because they
//! are policy (cost parameters, token length) rather than storage.
use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub created_at: DateTime<Utc>,
}

// The columns that may leave this module are spelled out in each query:
// `id, email, display_name, created_at`. `password_hash` is NOT among them, and
// that is enforced by the `User` type having no such field rather than by
// remembering to omit it at each call site — no handler can serialise a hash by
// accident, because there is nowhere to put one.
//
// (sqlx 0.9 requires `'static` SQL, so these cannot be assembled from a shared
// const via `format!`. The list is short and the type is the real guard.)

/// Create a user. `password_hash` must ALREADY be hashed — this function has no
/// way to tell a hash from a plaintext password and will happily store either,
/// so the type of the argument is the only guard and the caller owns it.
pub async fn create(
    pool: &sqlx::PgPool,
    email: &str,
    password_hash: &str,
    display_name: &str,
) -> Result<User, sqlx::Error> {
    sqlx::query_as::<_, User>(
        "INSERT INTO users (email, password_hash, display_name)
         VALUES ($1, $2, $3)
         RETURNING id, email, display_name, created_at",
    )
    .bind(email)
    .bind(password_hash)
    .bind(display_name)
    .fetch_one(pool)
    .await
}

/// Look a user up for sign-in, returning the stored hash alongside them.
///
/// Case-insensitive on email, matching `users_email_lower_idx` — a sign-in that
/// was case-sensitive while the uniqueness index was not would let
/// "A@b.com" fail to sign in to the account it collides with.
pub async fn find_for_signin(
    pool: &sqlx::PgPool,
    email: &str,
) -> Result<Option<(User, String)>, sqlx::Error> {
    let row: Option<(Uuid, String, String, DateTime<Utc>, String)> = sqlx::query_as(
        "SELECT id, email, display_name, created_at, password_hash
         FROM users WHERE lower(email) = lower($1)",
    )
    .bind(email)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(id, email, display_name, created_at, hash)| {
        (
            User {
                id,
                email,
                display_name,
                created_at,
            },
            hash,
        )
    }))
}

/// Record a session. `token_hash` is the hash of the token, never the token.
pub async fn create_session(
    pool: &sqlx::PgPool,
    token_hash: &str,
    user_id: Uuid,
    expires_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)",
    )
    .bind(token_hash)
    .bind(user_id)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// The user behind a live session, or `None`.
///
/// EXPIRY IS PART OF THE QUERY (`expires_at > now()`), not a background sweep.
/// A sweeper that fell behind — because the process died, or the schedule
/// slipped — would leave expired sessions accepting requests, and nothing would
/// report it. A predicate cannot fall behind.
pub async fn session_user(
    pool: &sqlx::PgPool,
    token_hash: &str,
) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as::<_, User>(
        "SELECT u.id, u.email, u.display_name, u.created_at
         FROM sessions s
         JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = $1 AND s.expires_at > now()",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await
}

/// Sign out. Idempotent: signing out twice is not an error.
pub async fn delete_session(pool: &sqlx::PgPool, token_hash: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(token_hash)
        .execute(pool)
        .await?;
    Ok(())
}

/// Housekeeping only — see `session_user` for why this is not what makes expiry
/// safe.
pub async fn delete_expired_sessions(pool: &sqlx::PgPool) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM sessions WHERE expires_at <= now()")
        .execute(pool)
        .await?
        .rows_affected())
}

//! Authentication: password hashing, session tokens, and the middleware that
//! makes every API route require one.
//!
//! # The shape of the guarantee
//!
//! The middleware is attached to a NESTED ROUTER covering all of `/api`, not to
//! an enumerated list of routes. That distinction is the whole design: a list
//! rots the moment someone adds a route and forgets to add it to the list, and
//! nothing fails when they do. Nesting means an unauthenticated request to any
//! `/api/*` path — including one that does not exist — is rejected before
//! routing resolves it, so a new route is protected by default rather than by
//! remembering.
//!
//! It also means an unauthenticated caller cannot tell a real route from a
//! fictional one: both are 401. That is deliberate.
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng};
use argon2::Argon2;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use chrono::{Duration, Utc};
use noted_db::users::{self, User};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::state::AppState;

/// How long a session lives. Long enough not to be a nuisance, short enough
/// that a token lifted from a machine is not permanent.
pub const SESSION_DAYS: i64 = 30;

/// The cookie the browser holds.
pub const SESSION_COOKIE: &str = "noted_session";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("password hashing failed")]
    Hash,
    #[error("that email is already registered")]
    EmailTaken,
    #[error("incorrect email or password")]
    BadCredentials,
}

/// Hash a password for storage.
///
/// Argon2id with the crate's default parameters, and a fresh random salt per
/// password (`SaltString::generate`) so two users choosing the same password do
/// not share a hash.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| AuthError::Hash)
}

/// Verify a password against a stored hash.
///
/// Returns `false` rather than an error for a wrong password; a malformed
/// stored hash is also `false`, because a hash we cannot parse must never be
/// treated as a match.
pub fn verify_password(password: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A fresh session token: 32 bytes of OS randomness, hex-encoded.
///
/// Returned as `(token, token_hash)`. The TOKEN goes to the browser and is
/// never stored; the HASH is stored and the token is never recoverable from it.
pub fn new_token() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let hash = hash_token(&token);
    (token, hash)
}

/// SHA-256 of a token. Not a password hash and deliberately not Argon2: a
/// 256-bit random token has no guessable structure to slow an attacker down
/// over, and this runs on every single request.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Register a user and hand back a session.
pub async fn sign_up(
    pool: &sqlx::PgPool,
    email: &str,
    password: &str,
    display_name: &str,
) -> Result<(User, String), AuthError> {
    let hash = hash_password(password)?;
    let user = match users::create(pool, email, &hash, display_name).await {
        Ok(u) => u,
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(AuthError::EmailTaken);
        }
        Err(e) => return Err(e.into()),
    };
    // A user with no workspace has nowhere to write and no way to make one
    // through the UI, so the account would be inert. Created here rather than
    // lazily on first use: "signed up but broken until you click something" is
    // a state worth not having.
    noted_db::workspaces::create(pool, &format!("{display_name}'s workspace"), user.id).await?;

    let token = issue_session(pool, user.id).await?;
    Ok((user, token))
}

/// Verify credentials and hand back a session.
pub async fn sign_in(
    pool: &sqlx::PgPool,
    email: &str,
    password: &str,
) -> Result<(User, String), AuthError> {
    let found = users::find_for_signin(pool, email).await?;

    // Verify against a DUMMY hash when the user does not exist, so that a
    // missing account and a wrong password take the same time. Skipping the
    // work for an unknown email turns sign-in into an account-enumeration
    // oracle measurable over the network.
    let Some((user, stored)) = found else {
        let _ = verify_password(password, DUMMY_HASH);
        return Err(AuthError::BadCredentials);
    };

    if !verify_password(password, &stored) {
        return Err(AuthError::BadCredentials);
    }

    let token = issue_session(pool, user.id).await?;
    Ok((user, token))
}

/// A real Argon2 hash of a value nobody knows, used only to spend the same time
/// verifying a nonexistent account as a real one. Generated once, offline.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHR2YWx1ZQ$Yx7cRZ8TQfMlNVJXvBLpZ8Z1Jj1cQzTQ0Vv3xX9pKfM";

pub async fn issue_session(pool: &sqlx::PgPool, user_id: uuid::Uuid) -> Result<String, AuthError> {
    let (token, token_hash) = new_token();
    let expires = Utc::now() + Duration::days(SESSION_DAYS);
    users::create_session(pool, &token_hash, user_id, expires).await?;
    Ok(token)
}

/// The `Set-Cookie` value for a freshly issued session.
///
/// `HttpOnly` so script cannot read it, `SameSite=Lax` so it does not ride
/// cross-site form posts, `Path=/` so the sync WebSocket sees it too. `Secure`
/// is added when the deployment is not plain-HTTP localhost — see
/// `cookie_is_secure`.
pub fn session_cookie(token: &str, secure: bool) -> String {
    let max_age = SESSION_DAYS * 24 * 60 * 60;
    let secure_flag = if secure { "; Secure" } else { "" };
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}{secure_flag}"
    )
}

/// The `Set-Cookie` that clears a session.
pub fn clear_cookie(secure: bool) -> String {
    let secure_flag = if secure { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0{secure_flag}")
}

/// Pull the session token out of a request's cookies.
pub fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.to_string())
}

/// The middleware. Rejects anything without a live session, and puts the `User`
/// into request extensions for handlers that want it.
pub async fn require_session(
    State(st): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Already authenticated upstream — an API token acts as its owner and has
    // put that `User` here (see `api_auth::token_or_session`). Demanding a
    // cookie as well would mean a token could never reach anything, which is
    // exactly what happened the first time these two layers were stacked.
    //
    // Checked by TYPE rather than by a flag: the only way a `User` gets into
    // extensions is for some layer to have proven identity, so this cannot be
    // spoofed by a header.
    if request.extensions().get::<User>().is_some() {
        return Ok(next.run(request).await);
    }

    let Some(token) = token_from_headers(request.headers()) else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let user = users::session_user(&st.pool, &hash_token(&token))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "session lookup failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let Some(user) = user else {
        // Expired or unknown. Same response either way: telling the caller
        // which one it was is free information about whether a token was ever
        // valid.
        return Err(StatusCode::UNAUTHORIZED);
    };

    request.extensions_mut().insert(user);
    Ok(next.run(request).await)
}

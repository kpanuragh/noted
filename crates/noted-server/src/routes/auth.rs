//! Sign-up, sign-in, sign-out, and "who am I".
//!
//! These are the ONLY `/api` routes reachable without a session, which is why
//! they are mounted on a separate public router rather than being excluded from
//! the protected one by name.
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::Json;
use noted_db::users::User;

use crate::auth::{self, AuthError};
use crate::state::AppState;

impl IntoResponse for AuthError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            // Both credential failures are 401 with the SAME message. Saying
            // "no such account" would turn sign-in into an account-enumeration
            // oracle, which is the same reason `sign_in` burns time on a dummy
            // hash for an unknown email.
            AuthError::BadCredentials => StatusCode::UNAUTHORIZED,
            AuthError::EmailTaken => StatusCode::CONFLICT,
            AuthError::Db(ref e) => {
                tracing::error!(error = %e, "auth database error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
            AuthError::Hash => {
                tracing::error!("password hashing failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        let body = match self {
            AuthError::Db(_) | AuthError::Hash => "something went wrong".to_string(),
            other => other.to_string(),
        };
        (status, Json(serde_json::json!({ "error": body }))).into_response()
    }
}

#[derive(serde::Deserialize)]
pub struct SignUpBody {
    pub email: String,
    pub password: String,
    pub display_name: Option<String>,
}

/// `POST /api/auth/signup`
pub async fn sign_up(
    State(st): State<AppState>,
    Json(body): Json<SignUpBody>,
) -> Result<impl IntoResponse, AuthError> {
    let email = body.email.trim();
    if email.is_empty() || !email.contains('@') {
        return Err(AuthError::BadCredentials);
    }
    // A minimum that is worth having without pretending to be a policy engine.
    // Length beats composition rules; a longer floor belongs in a config, not
    // a constant, and that arrives with the rest of M4.
    if body.password.chars().count() < 12 {
        return Err(AuthError::BadCredentials);
    }

    let display = body
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| email.split('@').next().unwrap_or("there"));

    let (user, token) = auth::sign_up(&st.pool, email, &body.password, display).await?;

    Ok((
        StatusCode::CREATED,
        [(
            header::SET_COOKIE,
            auth::session_cookie(&token, st.cookies_secure),
        )],
        Json(user),
    ))
}

#[derive(serde::Deserialize)]
pub struct SignInBody {
    pub email: String,
    pub password: String,
}

/// `POST /api/auth/signin`
pub async fn sign_in(
    State(st): State<AppState>,
    Json(body): Json<SignInBody>,
) -> Result<impl IntoResponse, AuthError> {
    let (user, token) = auth::sign_in(&st.pool, body.email.trim(), &body.password).await?;
    Ok((
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            auth::session_cookie(&token, st.cookies_secure),
        )],
        Json(user),
    ))
}

/// `POST /api/auth/signout`
///
/// Public rather than protected, and deliberately: signing out with an already
/// expired session must clear the cookie rather than 401, or a stale session
/// leaves the user stuck.
pub async fn sign_out(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AuthError> {
    if let Some(token) = auth::token_from_headers(&headers) {
        noted_db::users::delete_session(&st.pool, &auth::hash_token(&token)).await?;
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, auth::clear_cookie(st.cookies_secure))],
    ))
}

/// `GET /api/me` — protected, so reaching it at all proves the session is live.
pub async fn me(Extension(user): Extension<User>) -> Json<User> {
    Json(user)
}

//! Bearer-token authentication, and per-route scope enforcement.
//!
//! # The scope a route needs is DERIVED, not listed
//!
//! The obvious design is a table mapping each route to its scope. It rots the
//! moment someone adds a route and forgets the entry — and the failure is
//! silent and in the wrong direction: an unlisted route requires *nothing*.
//!
//! So the required scope is computed from the request itself:
//!
//!   * the RESOURCE is the first path segment after `/api`
//!   * the ACTION is `read` for GET/HEAD and `write` for everything else
//!
//! `GET /api/pages/{id}` needs `pages:read`; `POST /api/pages` needs
//! `pages:write`; `GET /api/ask/local` needs `ask:read`. A route added tomorrow
//! under `/api/exports` requires `exports:read` without anyone writing that
//! down — and, crucially, a token that was never granted `exports:*` cannot
//! reach it.
//!
//! The rule is the same shape as the auth middleware's: make the guarantee
//! structural, so forgetting is not a thing that can happen.
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use noted_db::api_tokens::TokenIdentity;

use crate::auth::hash_token;
use crate::state::AppState;

/// The scope a request requires, e.g. `pages:read`.
///
/// `None` for paths outside `/api`, which token auth does not cover.
pub fn required_scope(method: &Method, path: &str) -> Option<String> {
    let rest = path.strip_prefix("/api/")?;
    let resource = rest.split('/').next().filter(|s| !s.is_empty())?;
    let action = match *method {
        Method::GET | Method::HEAD | Method::OPTIONS => "read",
        _ => "write",
    };
    Some(format!("{resource}:{action}"))
}

/// Pull a bearer token out of the `Authorization` header.
pub fn bearer(request: &Request) -> Option<String> {
    let raw = request
        .headers()
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer "))?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Authenticate a bearer token and enforce its scope for this route.
///
/// Runs BEFORE the session middleware in the stack, and falls through when
/// there is no bearer header — so a browser session is unaffected and a token
/// is simply a second way in.
pub async fn token_or_session(
    State(st): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(token) = bearer(&request) else {
        // No bearer header: leave it to the session middleware.
        return Ok(next.run(request).await);
    };

    let identity = noted_db::api_tokens::resolve(&st.pool, &hash_token(&token))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "api token lookup failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // A path token auth does not understand is refused rather than allowed.
    // Failing open here would mean any future non-`/api` route is reachable by
    // any token at all.
    let scope = required_scope(request.method(), request.uri().path())
        .ok_or(StatusCode::FORBIDDEN)?;

    if !identity.allows(&scope) {
        return Err(StatusCode::FORBIDDEN);
    }

    // The session middleware downstream expects a `User`; a token acts as its
    // owner, so it puts the same type in. That means every existing handler,
    // permission check and ACL applies to a token exactly as to a browser —
    // a token can never reach more than the person who created it.
    let user = noted_db::users::session_user_by_id(&st.pool, identity.user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    request.extensions_mut().insert(user);
    request.extensions_mut().insert(TokenIdentity {
        user_id: identity.user_id,
        workspace_id: identity.workspace_id,
        scopes: identity.scopes,
    });
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scope_is_derived_from_the_path_and_method() {
        assert_eq!(required_scope(&Method::GET, "/api/pages").as_deref(), Some("pages:read"));
        assert_eq!(required_scope(&Method::POST, "/api/pages").as_deref(), Some("pages:write"));
        assert_eq!(
            required_scope(&Method::GET, "/api/pages/abc/related").as_deref(),
            Some("pages:read"),
            "nested paths take the FIRST segment, so a sub-route cannot invent a new scope"
        );
        assert_eq!(required_scope(&Method::GET, "/api/ask/local").as_deref(), Some("ask:read"));
        assert_eq!(required_scope(&Method::DELETE, "/api/shares/x").as_deref(), Some("shares:write"));
    }

    /// A route added tomorrow gets a scope automatically — that is the whole
    /// point of deriving rather than listing.
    #[test]
    fn a_route_nobody_has_written_yet_still_requires_a_scope() {
        assert_eq!(
            required_scope(&Method::GET, "/api/exports/2026").as_deref(),
            Some("exports:read")
        );
    }

    /// Non-`/api` paths yield no scope, and the middleware refuses them for
    /// tokens rather than letting them through.
    #[test]
    fn paths_outside_api_have_no_scope() {
        assert!(required_scope(&Method::GET, "/health").is_none());
        assert!(required_scope(&Method::GET, "/sync/abc").is_none());
    }
}

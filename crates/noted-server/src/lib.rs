pub mod error;
pub mod auth;
pub mod membership;
pub mod routes;
pub mod state;

pub use state::AppState;

use axum::Router;
use axum::http::{HeaderValue, Method};
use axum::routing::{get, post};
use tower_http::cors::CorsLayer;

fn cors_layer() -> CorsLayer {
    let origin = std::env::var("CORS_ALLOWED_ORIGIN")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());
    match origin.parse::<HeaderValue>() {
        // `allow_credentials` is what lets the session cookie travel at all.
        // The web app is on :3000 and the API on :8787, so every request is
        // cross-origin, and a browser sends cookies cross-origin ONLY when the
        // request sets `credentials: "include"` AND the response allows
        // credentials. Without this the whole app is signed out on every call
        // and the failure looks like a broken session rather than a CORS
        // setting.
        //
        // Note this is why the origin must stay an explicit value and can never
        // become `*`: the spec forbids credentials with a wildcard origin, and
        // tower-http panics rather than silently allowing it.
        Ok(value) => CorsLayer::new()
            .allow_origin(value)
            .allow_credentials(true)
            .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
            .allow_headers([axum::http::header::CONTENT_TYPE]),
        Err(_) => {
            tracing::warn!(%origin, "invalid CORS_ALLOWED_ORIGIN; refusing all cross-origin requests");
            CorsLayer::new()
        }
    }
}

/// # Everything under `/api` requires a session, BY CONSTRUCTION
///
/// The protected routes are nested under one router carrying the auth
/// middleware, rather than each route being listed somewhere as "needs auth".
/// A list is a thing you can forget to add to, and nothing fails when you do;
/// nesting means a route added tomorrow is protected the moment it is written.
///
/// The only exceptions are `/health` (a liveness probe must not need
/// credentials) and `/api/auth/*` (you cannot present a session before you have
/// one). Both are mounted separately and visibly.
///
/// `/sync/{page_id}` is protected too. A WebSocket upgrade carries cookies like
/// any other request, so the same middleware covers it — an unauthenticated
/// socket would otherwise stream a page's entire content.
pub fn app(state: AppState) -> Router {
    let public = Router::new()
        .route("/health", get(routes::health::health))
        .route("/api/auth/signup", post(routes::auth::sign_up))
        .route("/api/auth/signin", post(routes::auth::sign_in))
        .route("/api/auth/signout", post(routes::auth::sign_out))
        // PUBLIC by design: the recipient of a share link has no account. It
        // serves exactly the page the token names — see `routes::shares::read`.
        .route("/api/shared/{token}", get(routes::shares::read));

    let protected = Router::new()
        .route("/api/me", get(routes::auth::me))
        .route(
            "/api/workspaces",
            get(routes::workspaces::mine).post(routes::workspaces::create),
        )
        .route(
            "/api/pages",
            get(routes::pages::list).post(routes::pages::create),
        )
        // Declared before "/api/pages/{id}" for readability; axum matches the
        // static segment in preference to the dynamic one either way.
        .route("/api/pages/recent", get(routes::pages::recent))
        .route(
            "/api/pages/{id}",
            get(routes::pages::get).patch(routes::pages::rename),
        )
        .route("/api/pages/{id}/reproject", post(routes::pages::reproject))
        .route("/api/pages/{id}/related", get(routes::search::related))
        .route("/api/pages/{id}/share", post(routes::shares::create))
        .route("/api/shares/{token}", axum::routing::delete(routes::shares::revoke))
        .route("/api/quickfind", get(routes::search::quick_find))
        .route("/api/search", get(routes::search::search))
        .route("/api/ask/local", get(routes::ask::local))
        .route("/api/ask/global", get(routes::ask::global))
        .route(
            "/api/workspaces/{workspace_id}/stats",
            get(routes::workspaces::stats),
        )
        .route(
            "/api/workspaces/{workspace_id}/indexing",
            get(routes::workspaces::indexing),
        )
        .route("/sync/{page_id}", get(routes::sync::handler))
        // Anything that falls through to here is an unknown path UNDER the
        // protected router. It still passes through the middleware first, so an
        // unauthenticated caller gets 401 rather than 404 and cannot use the
        // difference to map which routes exist.
        .fallback(|| async { axum::http::StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_session,
        ));

    public
        .merge(protected)
        .layer(cors_layer())
        .with_state(state)
}

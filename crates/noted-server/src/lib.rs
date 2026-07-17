pub mod error;
pub mod routes;
pub mod state;

pub use state::AppState;

use axum::http::{HeaderValue, Method};
use axum::routing::get;
use axum::Router;
use tower_http::cors::CorsLayer;

fn cors_layer() -> CorsLayer {
    let origin = std::env::var("CORS_ALLOWED_ORIGIN")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());
    match origin.parse::<HeaderValue>() {
        Ok(value) => CorsLayer::new()
            .allow_origin(value)
            .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
            .allow_headers([axum::http::header::CONTENT_TYPE]),
        Err(_) => {
            tracing::warn!(%origin, "invalid CORS_ALLOWED_ORIGIN; refusing all cross-origin requests");
            CorsLayer::new()
        }
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(routes::health::health))
        .layer(cors_layer())
        .with_state(state)
}

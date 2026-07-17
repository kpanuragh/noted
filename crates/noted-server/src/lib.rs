pub mod error;
pub mod routes;
pub mod state;

pub use state::AppState;

use axum::routing::get;
use axum::Router;
use tower_http::cors::CorsLayer;

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(routes::health::health))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

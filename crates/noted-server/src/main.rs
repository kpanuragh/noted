use std::sync::Arc;

use noted_index::provider::FastEmbed;
use noted_server::{AppState, app};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());

    let pool = noted_db::connect(&url).await?;
    noted_db::migrate(&pool).await?;

    // Loaded exactly ONCE, here, at startup: `FastEmbed::new()` loads ~417MB of
    // ONNX model. Constructing it per request (or per connection) would make
    // every search take seconds and re-load the model every time. From here on
    // it is shared via `Arc` inside `AppState` for the life of the process.
    tracing::info!("loading embedding model (bge-base-en-v1.5)...");
    let embedder = Arc::new(FastEmbed::new()?);
    tracing::info!("embedding model loaded");

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("noted-server listening on {bind}");
    axum::serve(listener, app(AppState::new(pool, embedder))).await?;
    Ok(())
}

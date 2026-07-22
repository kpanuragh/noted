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

    // The background indexer. Without it, a page written through the editor is
    // invisible to search until a human runs the `noted-index` CLI — which, for
    // a product whose pitch is "ask your notes", is the difference between
    // working and not.
    //
    // Extraction stays opt-in via NOTED_EXTRACT, exactly as the CLI gates it:
    // there is no LLM in most deployments, and a stub quietly building a
    // meaningless graph is worse than no graph.
    let extractor = match std::env::var("NOTED_EXTRACT").ok().as_deref() {
        Some("stub") => {
            tracing::warn!(
                "NOTED_EXTRACT=stub: the background indexer will build the knowledge graph with \
                 the DETERMINISTIC STUB extractor, not a real model. Fine for exercising the \
                 pipeline; the resulting graph is not meaningful."
            );
            Some(Arc::new(noted_index::extract::StubExtractor::new())
                as Arc<dyn noted_index::extract::ExtractionProvider>)
        }
        _ => {
            tracing::info!(
                "no extraction provider configured (set NOTED_EXTRACT=stub for local testing); \
                 the background indexer will embed but not extract"
            );
            None
        }
    };

    let scheduler = noted_index::scheduler::Scheduler::start(
        pool.clone(),
        embedder.clone(),
        extractor,
    )?;
    tracing::info!("background indexer started");

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("noted-server listening on {bind}");
    axum::serve(listener, app(AppState::new(pool, embedder))).await?;

    // Reached on graceful shutdown: let the current pass finish rather than
    // tearing it out mid-write.
    scheduler.stop().await;
    Ok(())
}

use std::sync::Arc;

use noted_index::extract::ExtractionProvider;
use noted_index::provider::FastEmbed;
use noted_server::{AppState, app};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Before anything reads the environment. `dotenv` does NOT override
    // variables that are already set, which is the precedence you want: a
    // value exported in the shell, or injected by the container runtime, beats
    // the file on disk. A missing `.env` is not an error — it is the normal
    // case in a container, where the environment arrives from compose.
    let loaded = dotenvy::dotenv();
    tracing_subscriber::fmt::init();
    match loaded {
        Ok(path) => tracing::info!("loaded environment from {}", path.display()),
        Err(e) if e.not_found() => {}
        Err(e) => tracing::warn!("could not read .env: {e}"),
    }

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
    // All three model roles resolved from configuration, and a bad spec is
    // FATAL rather than a silent fall back to a stub — see providers.rs.
    let extractor = noted_server::providers::extractor().map_err(anyhow::Error::msg)?;
    let answerer = noted_server::providers::answerer().map_err(anyhow::Error::msg)?;
    let summariser = noted_server::providers::summariser().map_err(anyhow::Error::msg)?;

    // Taken from the extractor INSTANCE rather than re-derived from the
    // environment. `AppState` previously parsed NOTED_EXTRACT a second time to
    // fill `extract_model`, which meant the id the indexing status reported
    // against and the model actually doing the extracting were two independent
    // reads of the same variable — they agreed only by luck, and would diverge
    // the moment either parse changed.
    let extract_model = extractor.as_ref().map(|e| e.model_id().to_string());

    let scheduler = noted_index::scheduler::Scheduler::start(
        pool.clone(),
        embedder.clone(),
        extractor,
    )?;
    tracing::info!("background indexer started");

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("noted-server listening on {bind}");
    let mut state = AppState::new(pool, embedder);
    state.answerer = answerer;
    state.summariser = summariser;
    state.extract_model = extract_model;

    axum::serve(listener, app(state)).await?;

    // Reached on graceful shutdown: let the current pass finish rather than
    // tearing it out mid-write.
    scheduler.stop().await;
    Ok(())
}

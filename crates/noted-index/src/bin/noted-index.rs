use std::sync::Arc;

use noted_index::provider::{EmbeddingProvider, FastEmbed};
use noted_index::worker::Worker;

/// Drains the embedding queue and exits. Safe to kill and re-run at any point —
/// the queue is a set difference with no in-progress state.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await?;
    noted_db::migrate(&pool).await?;

    tracing::info!(
        "loading embedding model (first run downloads ~400MB of ONNX weights into \
         .fastembed_cache/; subsequent runs are fast)"
    );
    let provider = Arc::new(FastEmbed::new()?);
    let model_id = provider.model_id().to_string();
    let worker = Worker::new(pool.clone(), provider)?;

    // `None`: the CLI drains the whole instance, not one workspace.
    let (done, total) = noted_db::chunks::progress(&pool, &model_id, None).await?;
    tracing::info!(embedded = done, total, "starting");

    let n = worker.drain().await?;

    let (done, total) = noted_db::chunks::progress(&pool, &model_id, None).await?;
    tracing::info!(embedded_this_run = n, embedded = done, total, "done");
    Ok(())
}

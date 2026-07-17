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

    // Chunks are only materialised when a page is edited (via the debounced
    // projection) or explicitly reprojected. A corpus that predates this pipeline
    // — every page on an M1a instance — has blocks but no page_chunks, so the
    // queue would be empty and this CLI would report "done" having indexed
    // nothing. Rechunk everything first: it is idempotent and content-addressed,
    // so unchanged pages cost one no-op insert each.
    let pages = noted_db::pages::all_page_ids(&pool).await?;
    tracing::info!(pages = pages.len(), "materialising chunks");
    let mut chunked = 0usize;
    for page_id in &pages {
        match noted_index::materialize::rechunk_page(&pool, *page_id).await {
            Ok(n) => chunked += n,
            // Do not abort the whole backfill for one bad page.
            Err(e) => tracing::warn!(error = %e, %page_id, "rechunk failed; skipping"),
        }
    }
    tracing::info!(pages = pages.len(), chunks = chunked, "materialised");

    // `None`: the CLI drains the whole instance, not one workspace.
    let (done, total) = noted_db::chunks::progress(&pool, &model_id, None).await?;
    if total == 0 {
        // An empty queue after rechunking every page is NOT success, and must not
        // read like it. Either the instance has no pages, or none of them contain
        // any text to index — a user whose vault indexed nothing needs to be told,
        // not handed a cheerful "done".
        tracing::warn!(
            pages = pages.len(),
            "NOTHING TO INDEX: no chunks exist after materialising every page, so search \
             will return no results. Either this instance has no pages, or its pages have \
             no indexable text in them."
        );
        return Ok(());
    }
    tracing::info!(embedded = done, total, "starting");

    let n = worker.drain().await?;

    let (done, total) = noted_db::chunks::progress(&pool, &model_id, None).await?;
    tracing::info!(embedded_this_run = n, embedded = done, total, "done");
    Ok(())
}

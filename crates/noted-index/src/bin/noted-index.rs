use std::sync::Arc;

use noted_index::extract::{ExtractionProvider, StubExtractor};
use noted_index::extract_worker::ExtractWorker;
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

    // Extraction pass. There is no real (LLM-backed) `ExtractionProvider`
    // wired into this CLI yet — `extract_providers::OllamaExtractor` exists
    // behind the `extract-ollama` feature, but this binary is not built with
    // it, and even if it were, there is no Ollama server to point it at in
    // most environments this CLI runs in. Rather than either (a) silently
    // skipping extraction with no explanation, which would look like a
    // working pipeline that just never touches the graph, or (b) defaulting
    // to `StubExtractor` — a deterministic, fake extractor meant for
    // tests — and quietly writing made-up graph data into a real instance,
    // extraction only runs when explicitly opted into via `NOTED_EXTRACT=stub`.
    // Any other provider wiring (Ollama, etc.) is a future CLI flag, not a
    // silent default.
    match std::env::var("NOTED_EXTRACT").ok().as_deref() {
        Some("stub") => {
            tracing::warn!(
                "NOTED_EXTRACT=stub: using the deterministic StubExtractor, NOT a real \
                 extraction model. This is for local testing of the extraction pipeline only \
                 — do not rely on it for real graph data."
            );
            let extract_provider = Arc::new(StubExtractor::new());
            let extract_model_id = extract_provider.model_id().to_string();
            let extract_worker = ExtractWorker::new(pool.clone(), extract_provider);

            let (extracted_before, extract_total) =
                noted_db::graph::extraction_progress(&pool, &extract_model_id, None).await?;
            tracing::info!(extracted = extracted_before, total = extract_total, "extraction starting");

            match extract_worker.drain().await {
                Ok(n) => {
                    let (extracted, total) =
                        noted_db::graph::extraction_progress(&pool, &extract_model_id, None).await?;
                    tracing::info!(extracted_this_run = n, extracted, total, "extraction done");
                }
                // A stalled/failed extraction drain must not take down the
                // whole CLI run — embeddings already succeeded above, and that
                // work must not be thrown away because the graph pass hit a
                // poison chunk or (for a real provider) an unreachable model.
                Err(e) => {
                    tracing::warn!(error = %e, "extraction did not complete; embeddings above are unaffected");
                }
            }
        }
        _ => {
            tracing::info!(
                "no extraction provider configured (set NOTED_EXTRACT=stub to run the \
                 deterministic stub extractor for local testing); skipping the extraction pass"
            );
        }
    }

    Ok(())
}

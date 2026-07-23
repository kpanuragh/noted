//! Measuring what cosine distance actually looks like on real content.
//!
//! `search::hybrid`'s vector arm needs a distance cutoff, and the only honest
//! way to choose one is to measure it against real embeddings rather than pick
//! a number that sounds right. `#[ignore]`d because it needs a populated
//! database and loads the 417MB ONNX model.
//!
//! ```text
//! cargo test -p noted-index --features embed --test distance_calibration -- --ignored --nocapture
//! ```
#![cfg(feature = "embed")]

use noted_index::provider::{EmbeddingProvider, FastEmbed};

#[tokio::test]
#[ignore = "needs a populated database and loads the embedding model"]
async fn measure_distances_for_related_and_unrelated_queries() {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    let embedder = FastEmbed::new().unwrap();

    let queries = [
        "Data",              // the reported case: matches nothing lexically
        "asdfghjkl qwerty",  // pure nonsense
        "banking regulation compliance", // plausible English, unrelated content
        "dinosaurs",         // should genuinely match
        "Kerala beaches",    // should genuinely match
    ];

    println!("\n{:<34} {:>8} {:>8} {:>8} {:>10}", "query", "min", "p50", "max", "kept<0.45");
    for q in queries {
        let v = embedder.embed(&[q.to_string()]).await.unwrap().pop().unwrap();
        // Scoped to ONE workspace. Measuring across every embedding in a shared
        // dev database answers a different question than the one the bug is
        // about: a user searches inside their own workspace, and that is the
        // population a cutoff has to separate.
        let ws: uuid::Uuid = std::env::var("CALIBRATE_WORKSPACE")
            .expect("set CALIBRATE_WORKSPACE")
            .parse()
            .unwrap();
        let rows: Vec<(f64,)> = sqlx::query_as(
            "SELECT (e.embedding <=> $1)::float8 FROM embeddings e
             WHERE e.model_id = $2 AND EXISTS (
               SELECT 1 FROM page_chunks pc JOIN pages p ON p.id = pc.page_id
               WHERE pc.content_hash = e.content_hash AND p.workspace_id = $3)
             ORDER BY e.embedding <=> $1 LIMIT 40",
        )
        .bind(pgvector::Vector::from(v))
        .bind(embedder.model_id())
        .bind(ws)
        .fetch_all(&pool)
        .await
        .unwrap();
        if rows.is_empty() {
            println!("{q:<34}  (no embeddings in db)");
            continue;
        }
        let d: Vec<f64> = rows.iter().map(|r| r.0).collect();
        let kept = d.iter().filter(|x| **x < noted_db::search::MAX_COSINE_DISTANCE).count();
        println!(
            "{:<34} {:>8.3} {:>8.3} {:>8.3} {:>10}",
            q,
            d[0],
            d[d.len() / 2],
            d[d.len() - 1],
            kept
        );
    }
    println!();
}

//! Diagnosing the background summary queue against a real database.
//!
//! ```text
//! cargo test -p noted-index --features extract-ollama --test summary_live -- --ignored --nocapture
//! ```
#![cfg(feature = "extract-ollama")]

use std::sync::Arc;

#[tokio::test]
#[ignore = "needs a populated database and a running Ollama"]
async fn who_is_at_the_head_of_the_summary_queue() {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    let model_id = "ollama:llama3.2:1b";

    let heads =
        noted_index::summary_worker::workspaces_with_pending_summaries(&pool, model_id, 4)
            .await
            .unwrap();
    println!("queue head ({}):", heads.len());
    for ws in &heads {
        let (n, oldest): (i64, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
            "SELECT count(*), min(created_at) FROM communities WHERE workspace_id = $1",
        )
        .bind(ws)
        .fetch_one(&pool)
        .await
        .unwrap();
        let members: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM community_members cm
             JOIN communities c ON c.id = cm.community_id WHERE c.workspace_id = $1",
        )
        .bind(ws)
        .fetch_one(&pool)
        .await
        .unwrap();
        println!("  {ws}  communities={n} members={members} oldest={oldest}");
    }

    // Now actually run one pass for the FIRST workspace and report what happened.
    if let Some(ws) = heads.first() {
        let provider = Arc::new(
            noted_index::ollama::OllamaSummariser::new("http://localhost:11434", "llama3.2:1b")
                .unwrap(),
        );
        let worker =
            noted_index::summary_worker::SummaryWorker::new(pool.clone(), provider, *ws);
        match worker.run_once().await {
            Ok(pass) => println!("\nrun_once on {ws}: {pass:?}"),
            Err(e) => println!("\nrun_once on {ws} ERRORED: {e}"),
        }
    }
}

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


/// Summarise ONE named workspace now, regardless of its place in the
/// instance-wide queue.
///
/// The background pass is fair (oldest first) and this database is shared with
/// ~1800 test workspaces, so a real workspace can be hours down the queue. This
/// is the operator escape hatch: point it at a workspace and wait.
///
/// ```text
/// SUMMARISE_WORKSPACE=<uuid> cargo test -p noted-index --features extract-ollama \
///   --test summary_live summarise_one_workspace_now -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "operator tool: summarises a named workspace with a real model"]
async fn summarise_one_workspace_now() {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let pool = noted_db::connect(&url).await.unwrap();
    let ws: uuid::Uuid = std::env::var("SUMMARISE_WORKSPACE")
        .expect("set SUMMARISE_WORKSPACE=<uuid>")
        .parse()
        .unwrap();
    let model = std::env::var("SUMMARISE_MODEL").unwrap_or_else(|_| "llama3.2:1b".into());
    let base = std::env::var("NOTED_OLLAMA_URL")
        .unwrap_or_else(|_| "http://localhost:11434".into());

    let provider =
        Arc::new(noted_index::ollama::OllamaSummariser::new(&base, &model).unwrap());
    let worker = noted_index::summary_worker::SummaryWorker::new(pool.clone(), provider, ws);

    let pass = worker.run_once().await.expect("summary pass");
    println!("workspace {ws}: {pass:?}");

    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT s.summary FROM community_summaries s
         JOIN communities c ON c.id = s.community_id
         WHERE c.workspace_id = $1 ORDER BY c.level",
    )
    .bind(ws)
    .fetch_all(&pool)
    .await
    .unwrap();
    for (i, (summary,)) in rows.iter().enumerate() {
        println!("\n--- theme {} ---\n{}", i + 1, summary);
    }
    assert!(!rows.is_empty(), "no summary was written");
}

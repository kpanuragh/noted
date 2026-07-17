use noted_server::{app, AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted".into());
    let bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());

    let pool = noted_db::connect(&url).await?;
    noted_db::migrate(&pool).await?;

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("noted-server listening on {bind}");
    axum::serve(listener, app(AppState { pool })).await?;
    Ok(())
}

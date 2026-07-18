pub use sqlx::PgPool;

pub async fn connect(url: &str) -> Result<PgPool, sqlx::Error> {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(16)
        .connect(url)
        .await
}

pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

pub mod blocks;
pub mod chunks;
pub mod community;
pub mod docs;
pub mod graph;
pub mod pages;
pub mod search;
pub mod stats;

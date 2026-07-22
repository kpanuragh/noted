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
pub mod graph_search;
pub mod pages;
pub mod search;

// Identity: users and sessions. Primitives only — password and token hashing
// are policy and live in `noted-server`.
pub mod users;

// Workspaces and membership — who may name a given workspace_id.
pub mod workspaces;

// Per-page access control. Exports `readable_pages_cte!` — the ONE definition of
// "may this user read this page", spliced into every query returning page
// content rather than paraphrased in each.
pub mod acl;

// Share links: public, tokenised access to one page (and optionally its
// descendants). Never the workspace, never search.
pub mod shares;

// Databases: collections of pages with typed properties (M3).
pub mod collections;

// Saved views over a collection, and the filter/sort compiler behind them (M3).
pub mod views;

// Relations between database rows, and rollups computed over them (M3).
pub mod relations;
pub mod stats;

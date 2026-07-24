use sqlx::{Row, postgres::PgPoolOptions};

#[tokio::test]
async fn pgvector_extension_is_at_least_0_8() {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let pool = PgPoolOptions::new()
        .connect(&url)
        .await
        .expect("cannot connect to Postgres — is `docker compose up -d` running?");

    let row = sqlx::query("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
        .fetch_optional(&pool)
        .await
        .unwrap()
        .expect("vector extension not installed");

    let version: String = row.get("extversion");
    let mut parts = version.split('.');
    let major: u32 = parts.next().unwrap().parse().unwrap();
    let minor: u32 = parts.next().unwrap_or("0").parse().unwrap();

    // Hard requirement: iterative index scans (0.8.0+) — see spec §4.2.
    assert!(
        (major, minor) >= (0, 8),
        "pgvector {version} is below the required 0.8 — permission-filtered search would overfilter"
    );
}

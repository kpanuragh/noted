use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Page {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const COLS: &str = "id, workspace_id, parent_id, title, created_at, updated_at";

pub async fn create(
    pool: &PgPool,
    workspace_id: Uuid,
    parent_id: Option<Uuid>,
    title: &str,
) -> Result<Page, sqlx::Error> {
    // SAFETY: only COLS (a &'static str const) is interpolated; every runtime
    // value below is parameter-bound. Never wrap a string containing user input.
    sqlx::query_as::<_, Page>(sqlx::AssertSqlSafe(format!(
        "INSERT INTO pages (workspace_id, parent_id, title)
         VALUES ($1, $2, $3) RETURNING {COLS}"
    )))
    .bind(workspace_id)
    .bind(parent_id)
    .bind(title)
    .fetch_one(pool)
    .await
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<Page>, sqlx::Error> {
    // SAFETY: only COLS (a &'static str const) is interpolated; every runtime
    // value below is parameter-bound. Never wrap a string containing user input.
    sqlx::query_as::<_, Page>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM pages WHERE id = $1 AND archived_at IS NULL"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn children(
    pool: &PgPool,
    workspace_id: Uuid,
    parent_id: Option<Uuid>,
) -> Result<Vec<Page>, sqlx::Error> {
    // `IS NOT DISTINCT FROM` so a NULL parent_id matches root pages.
    // SAFETY: only COLS (a &'static str const) is interpolated; every runtime
    // value below is parameter-bound. Never wrap a string containing user input.
    sqlx::query_as::<_, Page>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM pages
         WHERE workspace_id = $1
           AND parent_id IS NOT DISTINCT FROM $2
           AND archived_at IS NULL
         ORDER BY created_at"
    )))
    .bind(workspace_id)
    .bind(parent_id)
    .fetch_all(pool)
    .await
}

pub async fn rename(pool: &PgPool, id: Uuid, title: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE pages SET title = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(title)
        .execute(pool)
        .await?;
    Ok(())
}

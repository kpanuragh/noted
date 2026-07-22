//! Per-page access control, and the ONE definition of "may this user read this
//! page".
//!
//! # Inheritance, expressed once
//!
//! A row in `page_acls` is an OVERRIDE at one point in the page tree, not a
//! grant per page. Effective access is the nearest override walking up to the
//! root; with none, workspace membership decides, which the caller has already
//! established before any of this runs.
//!
//! [`readable_pages_cte!`] is that rule as a single SQL string, spliced into
//! every query that returns page content. It is deliberately a macro for the
//! same reason `clusterable_edges_cte!` is: this codebase produced four
//! data-loss bugs from two queries disagreeing about what "live" meant, and a
//! permission predicate that means something slightly different in the search
//! query than in the page-fetch query is that bug with worse consequences.
//!
//! DO NOT paraphrase it. Splice it.
use uuid::Uuid;

/// A CTE named `readable_pages(page_id)`: every page in `$1` (workspace) that
/// user `$2` may read.
///
/// Expands to a string literal so `concat!` can assemble each query at compile
/// time — sqlx 0.9's `SqlSafeStr` bound rejects `format!`-built SQL.
///
/// The recursion walks DOWN from the roots carrying effective access, rather
/// than up from each page looking for an ancestor. Both express the same rule;
/// walking down visits each page once, while walking up re-walks the whole
/// ancestor chain per page.
#[macro_export]
macro_rules! readable_pages_cte {
    () => {
        "readable_pages AS (
             WITH RECURSIVE effective(id, access) AS (
                 SELECT p.id, COALESCE(a.access, 'read')
                 FROM pages p
                 LEFT JOIN page_acls a ON a.page_id = p.id AND a.user_id = $2
                 WHERE p.workspace_id = $1 AND p.parent_id IS NULL
               UNION ALL
                 SELECT c.id, COALESCE(a.access, e.access)
                 FROM effective e
                 JOIN pages c ON c.parent_id = e.id
                 LEFT JOIN page_acls a ON a.page_id = c.id AND a.user_id = $2
             )
             SELECT id AS page_id FROM effective WHERE access <> 'none'
         )"
    };
}

/// Effective access for one page, for one user.
///
/// The single-page answer to the same question `readable_pages_cte!` answers in
/// bulk. Both must agree; `the_bulk_and_single_page_checks_agree` is the test
/// that keeps them honest, because two implementations of one rule is exactly
/// the shape of this project's worst bugs.
pub async fn can_read(
    pool: &sqlx::PgPool,
    page_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let access: Option<String> = sqlx::query_scalar(
        "WITH RECURSIVE chain(id, parent_id, access, depth) AS (
             SELECT p.id, p.parent_id, a.access, 0
             FROM pages p
             LEFT JOIN page_acls a ON a.page_id = p.id AND a.user_id = $2
             WHERE p.id = $1
           UNION ALL
             SELECT pg.id, pg.parent_id, a.access, c.depth + 1
             FROM chain c
             JOIN pages pg ON pg.id = c.parent_id
             LEFT JOIN page_acls a ON a.page_id = pg.id AND a.user_id = $2
             WHERE c.access IS NULL
         )
         SELECT access FROM chain WHERE access IS NOT NULL ORDER BY depth LIMIT 1",
    )
    .bind(page_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    // No override anywhere up the chain means workspace membership decides, and
    // the caller has already checked that.
    Ok(access.as_deref() != Some("none"))
}

/// Grant or deny one user's access to one page (and, by inheritance, its
/// descendants until another override).
pub async fn set_access(
    pool: &sqlx::PgPool,
    page_id: Uuid,
    user_id: Uuid,
    access: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO page_acls (page_id, user_id, access) VALUES ($1, $2, $3)
         ON CONFLICT (page_id, user_id) DO UPDATE SET access = EXCLUDED.access",
    )
    .bind(page_id)
    .bind(user_id)
    .bind(access)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove an override, so the page inherits from its parent again.
pub async fn clear_access(
    pool: &sqlx::PgPool,
    page_id: Uuid,
    user_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM page_acls WHERE page_id = $1 AND user_id = $2")
        .bind(page_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Every page in `workspace_id` that `user_id` may read.
///
/// Uses the shared CTE, so it is the bulk rule by construction rather than by
/// resemblance.
pub async fn readable_pages(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(concat!(
        "WITH ",
        readable_pages_cte!(),
        " SELECT page_id FROM readable_pages"
    ))
    .bind(workspace_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
}

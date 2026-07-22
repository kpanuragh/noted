//! Relations between database rows, and rollups computed over them.
use serde_json::Value as Json;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum RelationError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("a relation must point at a live page in the same workspace")]
    BadTarget,
    #[error("unknown rollup function: {0}")]
    UnknownFunction(String),
    #[error("rollup config is missing {0}")]
    BadConfig(&'static str),
}

/// Link `from` to `to` under a relation property.
///
/// The target is CHECKED — it must be a live page in the same workspace as the
/// source. A relation pointing at an archived page, a page in another tenant,
/// or a uuid that names nothing would render as a broken row forever, and the
/// check costs one query at write against unbounded confusion at read.
pub async fn link(
    pool: &sqlx::PgPool,
    property_id: Uuid,
    from_page: Uuid,
    to_page: Uuid,
) -> Result<(), RelationError> {
    let ok: Option<bool> = sqlx::query_scalar(
        "SELECT true
         FROM pages a
         JOIN pages b ON b.workspace_id = a.workspace_id
         WHERE a.id = $1 AND b.id = $2
           AND a.archived_at IS NULL AND b.archived_at IS NULL",
    )
    .bind(from_page)
    .bind(to_page)
    .fetch_optional(pool)
    .await?;
    if ok.is_none() {
        return Err(RelationError::BadTarget);
    }

    sqlx::query(
        "INSERT INTO page_relations (property_id, from_page, to_page)
         VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(property_id)
    .bind(from_page)
    .bind(to_page)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn unlink(
    pool: &sqlx::PgPool,
    property_id: Uuid,
    from_page: Uuid,
    to_page: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM page_relations
         WHERE property_id = $1 AND from_page = $2 AND to_page = $3",
    )
    .bind(property_id)
    .bind(from_page)
    .bind(to_page)
    .execute(pool)
    .await?;
    Ok(())
}

/// What this row points at.
pub async fn forward(
    pool: &sqlx::PgPool,
    property_id: Uuid,
    from_page: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT r.to_page FROM page_relations r
         JOIN pages p ON p.id = r.to_page AND p.archived_at IS NULL
         WHERE r.property_id = $1 AND r.from_page = $2
         ORDER BY r.created_at, r.to_page",
    )
    .bind(property_id)
    .bind(from_page)
    .fetch_all(pool)
    .await
}

/// What points at this row — the other half of "bidirectional", and the reason
/// links are rows rather than a JSON array.
pub async fn backward(
    pool: &sqlx::PgPool,
    property_id: Uuid,
    to_page: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT r.from_page FROM page_relations r
         JOIN pages p ON p.id = r.from_page AND p.archived_at IS NULL
         WHERE r.property_id = $1 AND r.to_page = $2
         ORDER BY r.created_at, r.from_page",
    )
    .bind(property_id)
    .bind(to_page)
    .fetch_all(pool)
    .await
}

/// The aggregate a rollup applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollupFn {
    Count,
    Sum,
    Min,
    Max,
    Latest,
}

impl RollupFn {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "count" => Self::Count,
            "sum" => Self::Sum,
            "min" => Self::Min,
            "max" => Self::Max,
            "latest" => Self::Latest,
            _ => return None,
        })
    }
}

/// Recompute one rollup for one row and store the result.
///
/// # Why the value is MATERIALISED rather than computed at read
///
/// A table of 500 rows would otherwise be 500 aggregate queries, and a rollup
/// would be unusable in a filter or a sort — the two places people actually put
/// them. Storing it means a rollup is an ordinary property value everywhere
/// downstream, at the cost of recomputing when the related set changes.
///
/// `recompute_for_target` is what closes that cost: it is called when the OTHER
/// side changes, which is the direction people forget.
pub async fn recompute(
    pool: &sqlx::PgPool,
    rollup_property: Uuid,
    page_id: Uuid,
) -> Result<Json, RelationError> {
    let config: Json = sqlx::query_scalar(
        "SELECT config FROM collection_properties WHERE id = $1",
    )
    .bind(rollup_property)
    .fetch_one(pool)
    .await?;

    let via = config
        .get("via")
        .and_then(Json::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or(RelationError::BadConfig("via"))?;
    let func_str = config
        .get("function")
        .and_then(Json::as_str)
        .ok_or(RelationError::BadConfig("function"))?;
    let func = RollupFn::parse(func_str)
        .ok_or_else(|| RelationError::UnknownFunction(func_str.into()))?;

    // `target` is only needed by the aggregates that read a value; `count`
    // counts links and deliberately does not require one.
    let target = config
        .get("target")
        .and_then(Json::as_str)
        .and_then(|s| Uuid::parse_str(s).ok());

    let related = forward(pool, via, page_id).await?;

    let value = if related.is_empty() {
        // An empty relation rolls up to 0 for `count` and to null for the
        // rest. "No related rows" is not "sum of nothing is zero" for a min or
        // a max, where zero would be a value the data never contained.
        match func {
            RollupFn::Count => Json::from(0),
            _ => Json::Null,
        }
    } else {
        match func {
            RollupFn::Count => Json::from(related.len()),
            _ => {
                let target = target.ok_or(RelationError::BadConfig("target"))?;
                let values: Vec<Json> = sqlx::query_scalar(
                    "SELECT value FROM page_properties
                     WHERE property_id = $1 AND page_id = ANY($2) AND value <> 'null'::jsonb",
                )
                .bind(target)
                .bind(&related)
                .fetch_all(pool)
                .await?;

                match func {
                    RollupFn::Sum => {
                        let total: f64 = values.iter().filter_map(Json::as_f64).sum();
                        Json::from(total)
                    }
                    RollupFn::Min => values
                        .iter()
                        .filter_map(Json::as_f64)
                        .fold(None::<f64>, |a, b| Some(a.map_or(b, |a| a.min(b))))
                        .map(Json::from)
                        .unwrap_or(Json::Null),
                    RollupFn::Max => values
                        .iter()
                        .filter_map(Json::as_f64)
                        .fold(None::<f64>, |a, b| Some(a.map_or(b, |a| a.max(b))))
                        .map(Json::from)
                        .unwrap_or(Json::Null),
                    // The value of the most recently updated related row.
                    RollupFn::Latest => sqlx::query_scalar::<_, Json>(
                        "SELECT value FROM page_properties
                         WHERE property_id = $1 AND page_id = ANY($2)
                         ORDER BY updated_at DESC LIMIT 1",
                    )
                    .bind(target)
                    .bind(&related)
                    .fetch_optional(pool)
                    .await?
                    .unwrap_or(Json::Null),
                    RollupFn::Count => unreachable!("handled above"),
                }
            }
        }
    };

    sqlx::query(
        "INSERT INTO page_properties (page_id, property_id, value, updated_at)
         VALUES ($1, $2, $3, now())
         ON CONFLICT (page_id, property_id)
         DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
    )
    .bind(page_id)
    .bind(rollup_property)
    .bind(&value)
    .execute(pool)
    .await?;

    Ok(value)
}

/// Recompute every rollup that could be affected by a change to `page_id`.
///
/// Walks BACKWARD: the rows whose rollups depend on this one are the rows that
/// point AT it. Changing a task's points must update the project that rolls
/// them up, and that project is not reachable from the task by any forward
/// link — which is the direction it is easy to forget, and why this function
/// exists rather than leaving callers to work it out.
///
/// Depth is ONE deliberately. A rollup over a rollup would need a dependency
/// walk with cycle detection, and a relation cycle (A relates to B relates to
/// A) would hang it. One level covers every rollup this product can currently
/// express, and `a_relation_cycle_does_not_hang_the_rollup` proves the cycle
/// case terminates.
pub async fn recompute_for_target(
    pool: &sqlx::PgPool,
    changed_page: Uuid,
) -> Result<usize, RelationError> {
    // Every (rollup property, dependent page) pair reachable from this change.
    let affected: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT cp.id, r.from_page
         FROM page_relations r
         JOIN collection_properties cp
           ON cp.kind = 'rollup'
          AND (cp.config ->> 'via')::uuid = r.property_id
         WHERE r.to_page = $1",
    )
    .bind(changed_page)
    .fetch_all(pool)
    .await?;

    let mut n = 0;
    for (rollup_property, page) in affected {
        recompute(pool, rollup_property, page).await?;
        n += 1;
    }
    Ok(n)
}

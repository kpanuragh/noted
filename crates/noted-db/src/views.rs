//! Saved views over a collection, and the filter/sort compiler behind them.
//!
//! # Filters compile to SQL; they are never interpolated
//!
//! A filter arrives as structured JSON — property, operator, value — and is
//! turned into a parameterised predicate here. The value is ALWAYS bound, never
//! formatted into the string, so a filter value cannot become SQL. The property
//! is looked up by id against the collection's own properties, so it cannot
//! name a column that is not there.
//!
//! # And they run in the database, not the browser
//!
//! A ten-thousand-row collection must not ship to the client to be filtered
//! there. That is why `LIMIT` is applied after the predicates rather than
//! before, and why sorting is `ORDER BY` rather than `Array.sort`.
use serde_json::Value as Json;
use uuid::Uuid;

use crate::collections::PropertyKind;

#[derive(Debug, Clone, PartialEq, serde::Serialize, sqlx::FromRow)]
pub struct View {
    pub id: Uuid,
    pub collection_id: Uuid,
    pub name: String,
    pub kind: String,
    pub config: Json,
    pub filters: Json,
    pub sorts: Json,
    pub position: i32,
}

/// One row of a view: the page, plus its property values.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Row {
    pub page_id: Uuid,
    pub title: String,
    /// Property id -> value. Absent properties are absent, not null — the
    /// caller knows the column list and can tell "never set" from "set to
    /// null", which a board's "No value" column depends on.
    pub values: std::collections::HashMap<Uuid, Json>,
}

#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("unknown operator: {0}")]
    UnknownOperator(String),
    #[error("filter names a property that is not in this collection")]
    UnknownProperty,
    #[error("malformed filter: {0}")]
    Malformed(&'static str),
}

/// The operators a filter can use.
///
/// A closed set, matched exhaustively: an operator this does not recognise is
/// an error rather than a fallback, so a typo cannot silently widen a filter
/// into "match everything".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Neq,
    Contains,
    GreaterThan,
    LessThan,
    IsEmpty,
    IsNotEmpty,
}

impl Op {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "eq" => Self::Eq,
            "neq" => Self::Neq,
            "contains" => Self::Contains,
            "gt" => Self::GreaterThan,
            "lt" => Self::LessThan,
            "is_empty" => Self::IsEmpty,
            "is_not_empty" => Self::IsNotEmpty,
            _ => return None,
        })
    }
}

pub async fn create(
    pool: &sqlx::PgPool,
    collection_id: Uuid,
    name: &str,
    kind: &str,
    config: Json,
) -> Result<View, sqlx::Error> {
    sqlx::query_as::<_, View>(
        "INSERT INTO collection_views (collection_id, name, kind, config)
         VALUES ($1, $2, $3, $4)
         RETURNING id, collection_id, name, kind, config, filters, sorts, position",
    )
    .bind(collection_id)
    .bind(name)
    .bind(kind)
    .bind(config)
    .fetch_one(pool)
    .await
}

pub async fn set_filters_and_sorts(
    pool: &sqlx::PgPool,
    view_id: Uuid,
    filters: Json,
    sorts: Json,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE collection_views SET filters = $2, sorts = $3 WHERE id = $1")
        .bind(view_id)
        .bind(filters)
        .bind(sorts)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get(pool: &sqlx::PgPool, view_id: Uuid) -> Result<Option<View>, sqlx::Error> {
    sqlx::query_as::<_, View>(
        "SELECT id, collection_id, name, kind, config, filters, sorts, position
         FROM collection_views WHERE id = $1",
    )
    .bind(view_id)
    .fetch_optional(pool)
    .await
}

/// How many rows one view request may return.
///
/// Clamped in the repository, like every other limit in this codebase, so a
/// second entry point cannot forget it.
pub const MAX_ROWS: i64 = 500;

/// Run a view: every live page in the collection matching its filters, in its
/// sort order.
///
/// `user_id` applies page permissions — a database row is a page, so a row the
/// caller may not read must not appear in a table any more than it appears in
/// search.
pub async fn run(
    pool: &sqlx::PgPool,
    view: &View,
    workspace_id: Uuid,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<Row>, ViewError> {
    let limit = limit.clamp(1, MAX_ROWS);

    // The collection's own properties, so a filter cannot name one that is not
    // here — and so the compiler knows each property's kind.
    let props: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, kind FROM collection_properties WHERE collection_id = $1",
    )
    .bind(view.collection_id)
    .fetch_all(pool)
    .await?;

    let mut predicates: Vec<String> = Vec::new();
    let mut binds: Vec<Json> = Vec::new();

    for f in view.filters.as_array().unwrap_or(&Vec::new()) {
        let property_id = f
            .get("property")
            .and_then(Json::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or(ViewError::Malformed("filter needs a property uuid"))?;

        let kind_str = props
            .iter()
            .find(|(id, _)| *id == property_id)
            .map(|(_, k)| k.clone())
            .ok_or(ViewError::UnknownProperty)?;
        let kind = PropertyKind::parse(&kind_str).ok_or(ViewError::UnknownProperty)?;

        let op_str = f
            .get("op")
            .and_then(Json::as_str)
            .ok_or(ViewError::Malformed("filter needs an op"))?;
        let op = Op::parse(op_str).ok_or_else(|| ViewError::UnknownOperator(op_str.into()))?;

        // $1 workspace, $2 user, $3 collection, $4 limit are fixed; filter
        // values start at $5. Two binds per filter at most, so the index is
        // computed rather than assumed.
        let next = 5 + binds.len();
        let prop_lit = format!("'{property_id}'::uuid");

        let sql = match op {
            Op::IsEmpty => format!(
                "NOT EXISTS (SELECT 1 FROM page_properties v WHERE v.page_id = p.id \
                 AND v.property_id = {prop_lit} AND v.value <> 'null'::jsonb)"
            ),
            Op::IsNotEmpty => format!(
                "EXISTS (SELECT 1 FROM page_properties v WHERE v.page_id = p.id \
                 AND v.property_id = {prop_lit} AND v.value <> 'null'::jsonb)"
            ),
            Op::Eq | Op::Neq => {
                let value = f.get("value").cloned().unwrap_or(Json::Null);
                binds.push(value);
                let negate = if op == Op::Neq { "NOT " } else { "" };
                format!(
                    "{negate}EXISTS (SELECT 1 FROM page_properties v WHERE v.page_id = p.id \
                     AND v.property_id = {prop_lit} AND v.value = ${next}::jsonb)"
                )
            }
            Op::Contains => {
                // Text containment on the extracted string, and array
                // membership for multi-select — the same word means different
                // things for the two kinds, so the SQL differs rather than
                // pretending one shape fits both.
                let value = f.get("value").cloned().unwrap_or(Json::Null);
                binds.push(value);
                if kind == PropertyKind::MultiSelect {
                    format!(
                        "EXISTS (SELECT 1 FROM page_properties v WHERE v.page_id = p.id \
                         AND v.property_id = {prop_lit} AND v.value @> ${next}::jsonb)"
                    )
                } else {
                    format!(
                        "EXISTS (SELECT 1 FROM page_properties v WHERE v.page_id = p.id \
                         AND v.property_id = {prop_lit} \
                         AND (v.value #>> '{{}}') ILIKE '%' || (${next}::jsonb #>> '{{}}') || '%')"
                    )
                }
            }
            Op::GreaterThan | Op::LessThan => {
                let value = f.get("value").cloned().unwrap_or(Json::Null);
                binds.push(value);
                let cmp = if op == Op::GreaterThan { ">" } else { "<" };
                // Numbers compare numerically, everything else as text. A date
                // is RFC 3339 or YYYY-MM-DD, both of which sort correctly as
                // strings, which is why a date column needs no special case.
                if kind == PropertyKind::Number {
                    format!(
                        "EXISTS (SELECT 1 FROM page_properties v WHERE v.page_id = p.id \
                         AND v.property_id = {prop_lit} \
                         AND (v.value #>> '{{}}')::numeric {cmp} (${next}::jsonb #>> '{{}}')::numeric)"
                    )
                } else {
                    format!(
                        "EXISTS (SELECT 1 FROM page_properties v WHERE v.page_id = p.id \
                         AND v.property_id = {prop_lit} \
                         AND (v.value #>> '{{}}') {cmp} (${next}::jsonb #>> '{{}}'))"
                    )
                }
            }
        };
        predicates.push(sql);
    }

    // Sorts. Each names a property and a direction; anything else is ignored
    // rather than guessed at.
    let mut order: Vec<String> = Vec::new();
    for s in view.sorts.as_array().unwrap_or(&Vec::new()) {
        let Some(property_id) = s
            .get("property")
            .and_then(Json::as_str)
            .and_then(|v| Uuid::parse_str(v).ok())
        else {
            continue;
        };
        if !props.iter().any(|(id, _)| *id == property_id) {
            return Err(ViewError::UnknownProperty);
        }
        let dir = match s.get("direction").and_then(Json::as_str) {
            Some("desc") => "DESC",
            _ => "ASC",
        };
        // NULLS LAST in both directions: an unset cell belongs at the bottom of
        // a table whichever way the column is sorted, because "no value" is not
        // a small value.
        order.push(format!(
            "(SELECT v.value #>> '{{}}' FROM page_properties v \
              WHERE v.page_id = p.id AND v.property_id = '{property_id}'::uuid) {dir} NULLS LAST"
        ));
    }
    // A stable tiebreak, always. Without it two rows with equal sort keys can
    // swap places between requests and the table appears to shuffle itself.
    order.push("p.created_at ASC".into());
    order.push("p.id ASC".into());

    let where_clause = if predicates.is_empty() {
        String::new()
    } else {
        format!(" AND {}", predicates.join(" AND "))
    };

    let sql = format!(
        "WITH {readable}
         SELECT p.id, p.title
         FROM pages p
         JOIN readable_pages r ON r.page_id = p.id
         JOIN collections c ON c.id = $3
         WHERE p.workspace_id = $1
           AND p.archived_at IS NULL
           AND p.parent_id = c.page_id
           {where_clause}
         ORDER BY {order}
         LIMIT $4",
        readable = crate::readable_pages_cte!("$1", "$2"),
        where_clause = where_clause,
        order = order.join(", "),
    );

    // `AssertSqlSafe` because this string IS assembled at runtime — and the
    // audit sqlx is asking for is exactly what the code above does:
    //
    //   * every VALUE is bound (`$5`, `$6`, ...), never formatted in;
    //   * every OPERATOR comes from the closed `Op` enum, and an unrecognised
    //     one is an error rather than a fallback, so a typo cannot widen a
    //     filter into "match everything";
    //   * every PROPERTY id is a parsed `Uuid` that was found in this
    //     collection's own property list, so it cannot name anything else and
    //     cannot carry punctuation;
    //   * the sort DIRECTION is one of two literals.
    //
    // Nothing user-supplied reaches the string unparsed.
    let mut q = sqlx::query_as::<_, (Uuid, String)>(sqlx::AssertSqlSafe(sql.clone()))
        .bind(workspace_id)
        .bind(user_id)
        .bind(view.collection_id)
        .bind(limit);
    for b in &binds {
        q = q.bind(b);
    }
    let pages: Vec<(Uuid, String)> = q.fetch_all(pool).await?;

    let ids: Vec<Uuid> = pages.iter().map(|(id, _)| *id).collect();
    let values: Vec<(Uuid, Uuid, Json)> = sqlx::query_as(
        "SELECT page_id, property_id, value FROM page_properties WHERE page_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;

    let mut rows: Vec<Row> = pages
        .into_iter()
        .map(|(page_id, title)| Row {
            page_id,
            title,
            values: std::collections::HashMap::new(),
        })
        .collect();
    for (page_id, property_id, value) in values {
        if let Some(row) = rows.iter_mut().find(|r| r.page_id == page_id) {
            row.values.insert(property_id, value);
        }
    }
    Ok(rows)
}

/// Rows grouped for a board view.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Group {
    /// The group's value, or `None` for the "No value" column.
    pub value: Option<String>,
    pub rows: Vec<Row>,
}

/// Run a board view: the same rows a table would return, grouped by a select
/// property.
///
/// # "No value" is a COLUMN, not a hidden bucket
///
/// A row whose group property is unset must appear somewhere a user can see and
/// drag it out of. Dropping it — the obvious implementation, where you group by
/// a value and skip the ones that have none — makes rows silently vanish from
/// the board while still existing in the table, which reads as data loss.
///
/// The empty group is emitted even when it has no rows, so the column is a
/// visible drop target rather than something that appears only once something
/// is already in it.
pub async fn run_board(
    pool: &sqlx::PgPool,
    view: &View,
    workspace_id: Uuid,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<Group>, ViewError> {
    let group_by = view
        .config
        .get("group_by")
        .and_then(Json::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or(ViewError::Malformed("a board needs config.group_by"))?;

    // The declared options, in the order the property declares them — so board
    // columns keep a stable, meaningful order ("todo, doing, done") rather than
    // whatever order the data happened to arrive in.
    let config: Json = sqlx::query_scalar(
        "SELECT config FROM collection_properties WHERE id = $1 AND collection_id = $2",
    )
    .bind(group_by)
    .bind(view.collection_id)
    .fetch_optional(pool)
    .await?
    .ok_or(ViewError::UnknownProperty)?;

    let declared: Vec<String> = config
        .get("options")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let rows = run(pool, view, workspace_id, user_id, limit).await?;

    let mut groups: Vec<Group> = declared
        .into_iter()
        .map(|v| Group {
            value: Some(v),
            rows: Vec::new(),
        })
        .collect();
    groups.push(Group {
        value: None,
        rows: Vec::new(),
    });

    for row in rows {
        let key = row
            .values
            .get(&group_by)
            .and_then(|v| v.as_str().map(str::to_string));

        match groups.iter_mut().find(|g| g.value == key) {
            Some(g) => g.rows.push(row),
            // A value the property does not declare — a stale option someone
            // removed from the schema. It gets its own column rather than
            // disappearing, because the row still exists and someone has to be
            // able to move it out.
            None => groups.insert(
                groups.len() - 1,
                Group {
                    value: key,
                    rows: vec![row],
                },
            ),
        }
    }

    Ok(groups)
}

/// Move a row between board columns by setting its group property.
///
/// Setting `None` clears the value, which is how a row is dragged into the "No
/// value" column.
pub async fn move_to_group(
    pool: &sqlx::PgPool,
    view: &View,
    page_id: Uuid,
    value: Option<&str>,
) -> Result<(), ViewError> {
    let group_by = view
        .config
        .get("group_by")
        .and_then(Json::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or(ViewError::Malformed("a board needs config.group_by"))?;

    let json = value.map(Json::from).unwrap_or(Json::Null);
    crate::collections::set_value(pool, page_id, group_by, json)
        .await
        .map_err(|e| match e {
            crate::collections::PropertyError::Db(e) => ViewError::Db(e),
            _ => ViewError::Malformed("the group property rejected that value"),
        })
}

/// Rows for a calendar view, keyed by the date property the view names.
///
/// Rows with NO date are returned separately rather than dropped: a task with
/// no due date is exactly the task a user is looking for, and a calendar that
/// silently omits it hides work.
pub async fn run_calendar(
    pool: &sqlx::PgPool,
    view: &View,
    workspace_id: Uuid,
    user_id: Uuid,
    limit: i64,
) -> Result<(Vec<(String, Row)>, Vec<Row>), ViewError> {
    let date_prop = view
        .config
        .get("date_property")
        .and_then(Json::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or(ViewError::Malformed("a calendar needs config.date_property"))?;

    let rows = run(pool, view, workspace_id, user_id, limit).await?;
    let mut dated: Vec<(String, Row)> = Vec::new();
    let mut undated: Vec<Row> = Vec::new();

    for row in rows {
        match row.values.get(&date_prop).and_then(|v| v.as_str()) {
            // Truncated to a day: a calendar cell is a day, and an RFC 3339
            // timestamp and a bare date must land in the same cell.
            Some(s) => {
                let day = s.get(..10).unwrap_or(s).to_string();
                dated.push((day, row));
            }
            None => undated.push(row),
        }
    }

    dated.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.page_id.cmp(&b.1.page_id)));
    Ok((dated, undated))
}

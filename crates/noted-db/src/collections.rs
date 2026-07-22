//! Databases: collections, typed properties, and page property values.
use serde_json::Value as Json;
use uuid::Uuid;

/// The property types a column can have.
///
/// A Rust enum rather than a bare string at the boundary, so an unknown kind is
/// rejected once — here — instead of surfacing as a value nothing knows how to
/// render three layers later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyKind {
    Text,
    Number,
    Select,
    MultiSelect,
    Date,
    Checkbox,
    Url,
    Relation,
}

impl PropertyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Number => "number",
            Self::Select => "select",
            Self::MultiSelect => "multi_select",
            Self::Date => "date",
            Self::Checkbox => "checkbox",
            Self::Url => "url",
            Self::Relation => "relation",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "text" => Self::Text,
            "number" => Self::Number,
            "select" => Self::Select,
            "multi_select" => Self::MultiSelect,
            "date" => Self::Date,
            "checkbox" => Self::Checkbox,
            "url" => Self::Url,
            "relation" => Self::Relation,
            _ => return None,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PropertyError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("unknown property kind: {0}")]
    UnknownKind(String),
    #[error("{property}: expected {expected}, got {got}")]
    WrongType {
        property: String,
        expected: &'static str,
        got: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct Collection {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub page_id: Uuid,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, sqlx::FromRow)]
pub struct Property {
    pub id: Uuid,
    pub collection_id: Uuid,
    pub name: String,
    pub kind: String,
    pub config: Json,
    pub position: i32,
}

pub async fn create_collection(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    page_id: Uuid,
    name: &str,
) -> Result<Collection, sqlx::Error> {
    sqlx::query_as::<_, Collection>(
        "INSERT INTO collections (workspace_id, page_id, name)
         VALUES ($1, $2, $3)
         RETURNING id, workspace_id, page_id, name",
    )
    .bind(workspace_id)
    .bind(page_id)
    .bind(name)
    .fetch_one(pool)
    .await
}

pub async fn add_property(
    pool: &sqlx::PgPool,
    collection_id: Uuid,
    name: &str,
    kind: PropertyKind,
    config: Json,
    position: i32,
) -> Result<Property, sqlx::Error> {
    sqlx::query_as::<_, Property>(
        "INSERT INTO collection_properties (collection_id, name, kind, config, position)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id, collection_id, name, kind, config, position",
    )
    .bind(collection_id)
    .bind(name)
    .bind(kind.as_str())
    .bind(config)
    .bind(position)
    .fetch_one(pool)
    .await
}

pub async fn properties(
    pool: &sqlx::PgPool,
    collection_id: Uuid,
) -> Result<Vec<Property>, sqlx::Error> {
    sqlx::query_as::<_, Property>(
        "SELECT id, collection_id, name, kind, config, position
         FROM collection_properties
         WHERE collection_id = $1
         ORDER BY position, name",
    )
    .bind(collection_id)
    .fetch_all(pool)
    .await
}

/// Reject a value that does not match its property's declared type.
///
/// # Why this runs at WRITE, not at read
///
/// A wrong-typed value that reaches storage is a bug every reader then has to
/// cope with, forever, and the reader has no idea who wrote it or when. Rejecting
/// at the boundary means the only values in the table are values the schema
/// says are possible — so a view can render `number` as a number without
/// defensive parsing.
///
/// `null` is always accepted: an empty cell is a legitimate state for every
/// kind, and it is how a value is cleared.
pub fn validate(kind: PropertyKind, name: &str, value: &Json) -> Result<(), PropertyError> {
    if value.is_null() {
        return Ok(());
    }

    let ok = match kind {
        // A relation stores the related page's uuid, and a url and a select
        // option are both strings — but they are validated as strings only.
        // Checking that a relation's uuid names a live page would be a
        // foreign-key check done in the wrong layer and one round trip per
        // cell; `relations_are_checked_against_real_pages` in M3-4 is where
        // that belongs.
        PropertyKind::Text | PropertyKind::Url | PropertyKind::Select | PropertyKind::Relation => {
            value.is_string()
        }
        PropertyKind::Number => value.is_number(),
        PropertyKind::Checkbox => value.is_boolean(),
        // An RFC 3339 string. Stored as text rather than a Postgres date so a
        // date property can hold a date, a datetime, or a partial date without
        // three columns.
        PropertyKind::Date => value.as_str().is_some_and(|s| {
            chrono::DateTime::parse_from_rfc3339(s).is_ok()
                || chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
        }),
        PropertyKind::MultiSelect => value
            .as_array()
            .is_some_and(|items| items.iter().all(Json::is_string)),
    };

    if ok {
        Ok(())
    } else {
        Err(PropertyError::WrongType {
            property: name.to_string(),
            expected: kind.as_str(),
            got: describe(value),
        })
    }
}

fn describe(v: &Json) -> String {
    match v {
        Json::Null => "null".into(),
        Json::Bool(_) => "a boolean".into(),
        Json::Number(_) => "a number".into(),
        Json::String(_) => "a string".into(),
        Json::Array(_) => "an array".into(),
        Json::Object(_) => "an object".into(),
    }
}

/// Set one page's value for one property, validating it against the declared
/// kind first.
///
/// Looks the property up rather than taking the kind from the caller: a caller
/// that passes both the property id and its kind can pass a mismatched pair,
/// and then validation checks the wrong rule while looking like it checked.
pub async fn set_value(
    pool: &sqlx::PgPool,
    page_id: Uuid,
    property_id: Uuid,
    value: Json,
) -> Result<(), PropertyError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT kind, name FROM collection_properties WHERE id = $1")
            .bind(property_id)
            .fetch_optional(pool)
            .await?;

    let (kind_str, name) = row.ok_or(sqlx::Error::RowNotFound)?;
    let kind =
        PropertyKind::parse(&kind_str).ok_or_else(|| PropertyError::UnknownKind(kind_str.clone()))?;

    validate(kind, &name, &value)?;

    sqlx::query(
        "INSERT INTO page_properties (page_id, property_id, value, updated_at)
         VALUES ($1, $2, $3, now())
         ON CONFLICT (page_id, property_id)
         DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
    )
    .bind(page_id)
    .bind(property_id)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Every property value for one page, keyed by property id.
pub async fn values_for_page(
    pool: &sqlx::PgPool,
    page_id: Uuid,
) -> Result<Vec<(Uuid, Json)>, sqlx::Error> {
    sqlx::query_as("SELECT property_id, value FROM page_properties WHERE page_id = $1")
        .bind(page_id)
        .fetch_all(pool)
        .await
}

/// Delete a property, and with it every value anyone ever set for it.
///
/// The values go by database CASCADE rather than by a second statement here —
/// so they cannot be orphaned by a crash between two writes, and cannot be
/// missed by a future caller who deletes the row directly.
pub async fn delete_property(pool: &sqlx::PgPool, property_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM collection_properties WHERE id = $1")
        .bind(property_id)
        .execute(pool)
        .await?;
    Ok(())
}

//! Share links — the one PUBLIC content surface.
//!
//! Creating and revoking a link is protected (you must be able to read the page
//! to share it). READING one is not: the whole point is that the recipient has
//! no account.
use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use noted_db::users::User;
use uuid::Uuid;

use crate::auth::hash_token;
use crate::error::AppError;
use crate::membership::MemberPage;
use crate::state::AppState;

#[derive(serde::Deserialize)]
pub struct CreateShareBody {
    #[serde(default)]
    pub include_descendants: bool,
    /// Hours until the link dies. `None` means it does not expire.
    #[serde(default)]
    pub expires_in_hours: Option<i64>,
}

#[derive(serde::Serialize)]
pub struct CreatedShare {
    /// The ONLY time the token is ever readable. It is stored hashed, so a
    /// caller that loses this must create a new link rather than recover it.
    pub token: String,
    pub url_path: String,
}

/// `POST /api/pages/{id}/share`
///
/// Protected by `MemberPage`, which means the caller must be a workspace member
/// AND able to read the page under its ACL. Someone who cannot open a page must
/// not be able to publish it.
pub async fn create(
    State(st): State<AppState>,
    MemberPage(page_id): MemberPage,
    Extension(user): Extension<User>,
    Json(body): Json<CreateShareBody>,
) -> Result<(StatusCode, Json<CreatedShare>), AppError> {
    let (token, token_hash) = crate::auth::new_token();
    let expires_at = body
        .expires_in_hours
        .map(|h| chrono::Utc::now() + chrono::Duration::hours(h));

    noted_db::shares::create(
        &st.pool,
        &token_hash,
        page_id,
        user.id,
        body.include_descendants,
        expires_at,
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(CreatedShare {
            url_path: format!("/shared/{token}"),
            token,
        }),
    ))
}

/// `DELETE /api/shares/{token}` — revoke.
///
/// Takes the token rather than an id so the share dialog can revoke what it
/// just handed out. Protected: a stranger holding a link must not be able to
/// revoke it (or anyone else's).
pub async fn revoke(
    State(st): State<AppState>,
    Extension(_user): Extension<User>,
    Path(token): Path<String>,
) -> Result<StatusCode, AppError> {
    let existed = noted_db::shares::revoke(&st.pool, &hash_token(&token)).await?;
    if existed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

#[derive(serde::Serialize)]
pub struct SharedPage {
    pub id: Uuid,
    pub title: String,
    pub blocks: Vec<SharedBlock>,
}

#[derive(serde::Serialize)]
pub struct SharedBlock {
    pub node_type: String,
    pub text: String,
}

/// `GET /api/shared/{token}` — PUBLIC.
///
/// Returns exactly the page the token names, and its descendants only if the
/// link said so. Never the workspace, never search, never a sibling.
///
/// An unknown, expired or revoked token is **404**, identical to a token that
/// never existed. Distinguishing them would tell a stranger that a link was
/// once real, which is the beginning of guessing at others.
pub async fn read(
    State(st): State<AppState>,
    Path(token): Path<String>,
) -> Result<Json<Vec<SharedPage>>, AppError> {
    let link = noted_db::shares::resolve(&st.pool, &hash_token(&token))
        .await?
        .ok_or(AppError::NotFound)?;

    let ids = noted_db::shares::shared_page_ids(&st.pool, &link).await?;

    let rows: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT p.id, p.title, COALESCE(b.node_type, ''), COALESCE(b.text, '')
         FROM pages p
         LEFT JOIN blocks b ON b.page_id = p.id
         WHERE p.id = ANY($1)
         ORDER BY p.created_at, p.id, b.block_index",
    )
    .bind(&ids)
    .fetch_all(&st.pool)
    .await?;

    let mut out: Vec<SharedPage> = Vec::new();
    for (id, title, node_type, text) in rows {
        if out.last().map(|p| p.id) != Some(id) {
            out.push(SharedPage {
                id,
                title,
                blocks: Vec::new(),
            });
        }
        if !text.is_empty() {
            out.last_mut().unwrap().blocks.push(SharedBlock { node_type, text });
        }
    }
    Ok(Json(out))
}

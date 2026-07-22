//! Authorization: proving the caller may name the workspace they asked for.
//!
//! # Why this is an extractor and not a function you remember to call
//!
//! Authentication (`auth.rs`) is structural — the middleware covers a whole
//! router, so a new route is protected without anyone remembering. Authorization
//! needs the same property, and a `require_member(&pool, user, ws)?` line at the
//! top of each handler does not have it: the day someone writes a handler and
//! forgets the line, nothing fails and a tenancy hole ships silently.
//!
//! So the workspace id arrives as [`MemberWorkspace`], an extractor that
//! verifies membership while producing the value. A handler cannot obtain a
//! `workspace_id` from a request without that check having happened, because
//! there is no other way to get one — the raw `Uuid` never reaches the handler.
//! The type is the guarantee.
//!
//! The same idea covers page-addressed routes via [`MemberPage`], which resolves
//! the page's workspace and checks membership against that.
use axum::extract::{FromRequestParts, Path, Query};
use axum::http::StatusCode;
use axum::http::request::Parts;
use noted_db::users::User;
use noted_db::workspaces;
use uuid::Uuid;

use crate::state::AppState;

/// A workspace id the caller has been PROVEN to belong to.
///
/// Read from the `workspace_id` query parameter. Rejects with 400 when absent
/// or malformed, 403 when the caller is not a member.
#[derive(Debug, Clone, Copy)]
pub struct MemberWorkspace(pub Uuid);

#[derive(serde::Deserialize)]
struct WorkspaceQuery {
    workspace_id: Uuid,
}

impl FromRequestParts<AppState> for MemberWorkspace {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let Query(q) = Query::<WorkspaceQuery>::from_request_parts(parts, st)
            .await
            .map_err(|_| StatusCode::BAD_REQUEST)?;

        check(parts, st, q.workspace_id).await?;
        Ok(MemberWorkspace(q.workspace_id))
    }
}

/// A page id whose workspace the caller has been PROVEN to belong to.
///
/// 404 when the page does not exist, which is also what a non-member gets —
/// telling a stranger that a page id is real, just not theirs, is free
/// information about what exists on the server.
#[derive(Debug, Clone, Copy)]
pub struct MemberPage(pub Uuid);

impl FromRequestParts<AppState> for MemberPage {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let Path(page_id) = Path::<Uuid>::from_request_parts(parts, st)
            .await
            .map_err(|_| StatusCode::BAD_REQUEST)?;

        let workspace_id = workspaces::workspace_of_page(&st.pool, page_id)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "page workspace lookup failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
            .ok_or(StatusCode::NOT_FOUND)?;

        // NOT_FOUND, not FORBIDDEN: a non-member must not learn that this id
        // names a real page.
        check(parts, st, workspace_id)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;

        // Membership got them into the workspace; the page ACL decides whether
        // they may see THIS page (M4-3). Checked here rather than in each
        // handler for the same reason membership is: a handler written tomorrow
        // inherits it, and one written by someone who forgot cannot leak.
        //
        // Same 404 as a non-member, deliberately: "exists but not for you" is
        // information about what is in the workspace.
        let user = parts
            .extensions
            .get::<User>()
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let readable = noted_db::acl::can_read(&st.pool, page_id, user.id)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "page acl lookup failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        if !readable {
            return Err(StatusCode::NOT_FOUND);
        }

        Ok(MemberPage(page_id))
    }
}

/// The shared check. Both extractors route through it so there is exactly one
/// definition of "may this caller see this workspace".
async fn check(parts: &Parts, st: &AppState, workspace_id: Uuid) -> Result<(), StatusCode> {
    // The auth middleware put this here. Its absence means the route was
    // mounted outside the protected router, which is a wiring bug rather than a
    // client error — fail closed and say so loudly.
    let Some(user) = parts.extensions.get::<User>() else {
        tracing::error!(
            "membership check on a route with no authenticated user; it is mounted outside the \
             protected router"
        );
        return Err(StatusCode::UNAUTHORIZED);
    };

    let member = workspaces::is_member(&st.pool, workspace_id, user.id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "membership lookup failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if member {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

/// As [`MemberWorkspace`], but for routes where the workspace id is a PATH
/// segment rather than a query parameter — `/api/workspaces/{id}/stats`.
///
/// A separate type rather than one extractor that tries both: an extractor that
/// silently falls back from path to query would accept
/// `/api/workspaces/{mine}/stats?workspace_id={yours}` and check the wrong one.
/// Two explicit types make the source of the id part of the handler's signature.
#[derive(Debug, Clone, Copy)]
pub struct MemberWorkspacePath(pub Uuid);

impl FromRequestParts<AppState> for MemberWorkspacePath {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let Path(workspace_id) = Path::<Uuid>::from_request_parts(parts, st)
            .await
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        check(parts, st, workspace_id).await?;
        Ok(MemberWorkspacePath(workspace_id))
    }
}

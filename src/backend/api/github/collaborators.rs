//! Invitations and effective collaborator permissions, including organization and team grants.

use crate::backend::api::github::optional_json::OptionalJson;
use crate::backend::api::github::pagination::Pagination;
use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use crate::backend::repositories::collaborators::Error as MemberError;

use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, put, MethodRouter},
    Json,
};
use codeyard_db_core::models::AccessLevel;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

fn member_error(error: MemberError) -> Error {
    match error {
        MemberError::Denied => {
            Error::new(StatusCode::FORBIDDEN, "Repository administration required")
        }
        MemberError::NotFound => Error::missing(),
        MemberError::Invalid(message) => Error::invalid(message),
        MemberError::Unavailable => Error::unavailable(()),
    }
}

async fn invitation_document(
    state: &AppState,
    invitation: &crate::backend::repositories::collaborators::Invitation,
) -> Result<Value> {
    let repo = codeyard_db_repos::repositories::records::get_repository_by_id(
        &state.pool,
        invitation.repo_id,
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(Error::missing)?;
    let owner =
        codeyard_db_repos::repositories::records::repository_owner_name(&state.pool, repo.id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    let inviter =
        codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, invitation.inviter_id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    let invitee =
        codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, invitation.invitee_id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    Ok(crate::backend::webhooks::payloads::repository_invitation(
        invitation,
        crate::backend::webhooks::payloads::repository(&repo, &owner),
        &crate::backend::webhooks::payloads::Person::of(&inviter),
        &crate::backend::webhooks::payloads::Person::of(&invitee),
    ))
}

#[derive(Deserialize)]
struct List {
    affiliation: Option<String>,
    permission: Option<String>,
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<List>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let actor = auth
        .ok_or_else(crate::backend::api::github::auth::required)?
        .user;
    let affiliation = query.affiliation.as_deref().unwrap_or("all");
    if !matches!(affiliation, "all" | "direct" | "outside") {
        return Err(Error::invalid("Invalid affiliation"));
    }
    let permission = query
        .permission
        .as_deref()
        .map(|name| {
            crate::backend::repositories::collaborators::permission(name)
                .ok_or_else(|| Error::invalid("Invalid permission"))
        })
        .transpose()?;
    let (offset, limit) = page.window()?;
    let rows = crate::backend::repositories::collaborators::list(
        &state.pool,
        actor.id,
        repo.id,
        &crate::backend::repositories::collaborators::CollaboratorQuery {
            username: None,
            affiliation,
            permission: permission.map(|value| value as i16),
            offset: offset as i64,
            limit: limit as i64 + 1,
        },
    )
    .await
    .map_err(member_error)?;
    let next = rows.len() > limit;
    page.response(
        &uri,
        rows.into_iter()
            .take(limit)
            .map(|row| {
                crate::backend::webhooks::payloads::collaborator(
                    &row.user,
                    AccessLevel::from(row.permission),
                )
            })
            .collect(),
        next,
    )
}

async fn check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, username)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let actor = auth
        .ok_or_else(crate::backend::api::github::auth::required)?
        .user;
    let rows = crate::backend::repositories::collaborators::list(
        &state.pool,
        actor.id,
        repo.id,
        &crate::backend::repositories::collaborators::CollaboratorQuery {
            username: Some(&username),
            affiliation: "all",
            permission: None,
            offset: 0,
            limit: 1,
        },
    )
    .await
    .map_err(member_error)?;
    if rows.is_empty() {
        return Err(Error::missing());
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn permission(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, username)): Path<(String, String, String)>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    auth.ok_or_else(crate::backend::api::github::auth::required)?;
    let user = codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let level = crate::backend::repositories::access::user_access(&state.pool, &user, &repo)
        .await
        .map_err(Error::unavailable)?;
    let permission = if level.can_admin() {
        "admin"
    } else if level.can_write() {
        "write"
    } else if level.can_read() {
        "read"
    } else {
        "none"
    };
    Ok(Json(
        json!({"permission":permission,"role_name":if level.can_admin() {"admin"} else {level.as_str()},"user":payloads::collaborator(&user,level)}),
    ))
}

#[derive(Default, Deserialize)]
struct Add {
    permission: Option<String>,
}
async fn add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, username)): Path<(String, String, String)>,
    OptionalJson(input): OptionalJson<Add>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth
        .ok_or_else(crate::backend::api::github::auth::required)?
        .user;
    let input = input.unwrap_or_default();
    let level = crate::backend::repositories::collaborators::permission(
        input.permission.as_deref().unwrap_or("push"),
    )
    .ok_or_else(|| Error::invalid("Invalid permission"))?;
    match crate::backend::repositories::collaborators::add(
        &state.pool,
        actor.id,
        repo.id,
        &username,
        level,
    )
    .await
    .map_err(member_error)?
    {
        crate::backend::repositories::collaborators::Added::Updated => {
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        crate::backend::repositories::collaborators::Added::Invited(invitation) => Ok((
            StatusCode::CREATED,
            Json(invitation_document(&state, &invitation).await?),
        )
            .into_response()),
    }
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, username)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth
        .ok_or_else(crate::backend::api::github::auth::required)?
        .user;
    crate::backend::repositories::collaborators::remove(&state.pool, actor.id, repo.id, &username)
        .await
        .map_err(member_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn invitations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    params: Option<Path<HashMap<String, String>>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, actor) =
        if let Some(Path(params)) = params.filter(|Path(params)| params.contains_key("owner")) {
            let (repo, auth) =
                repository(&state, &headers, &params["owner"], &params["repo"], false).await?;
            (
                Some(repo.id),
                auth.ok_or_else(crate::backend::api::github::auth::required)?
                    .user,
            )
        } else {
            (
                None,
                crate::backend::api::github::auth::authenticate(&state, &headers, true)
                    .await?
                    .ok_or_else(crate::backend::api::github::auth::required)?
                    .user,
            )
        };
    let (offset, limit) = page.window()?;
    let rows = crate::backend::repositories::collaborators::invitations(
        &state.pool,
        actor.id,
        repo,
        offset as i64,
        limit as i64 + 1,
    )
    .await
    .map_err(member_error)?;
    let next = rows.len() > limit;
    let mut values = Vec::new();
    for invitation in rows.into_iter().take(limit) {
        values.push(invitation_document(&state, &invitation).await?);
    }
    page.response(&uri, values, next)
}

async fn respond(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    method: Method,
) -> Result<StatusCode> {
    let actor = crate::backend::api::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(crate::backend::api::github::auth::required)?;
    actor.require_write()?;
    crate::backend::repositories::collaborators::respond(
        &state.pool,
        actor.database_actor(),
        id,
        method == Method::PATCH,
    )
    .await
    .map_err(member_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct InvitationUpdate {
    permissions: String,
}
async fn update_invitation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Json(input): Json<InvitationUpdate>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth
        .ok_or_else(crate::backend::api::github::auth::required)?
        .user;
    let level = crate::backend::repositories::collaborators::permission(&input.permissions)
        .ok_or_else(|| Error::invalid("Invalid permission"))?;
    let invitation = crate::backend::repositories::collaborators::update_invitation(
        &state.pool,
        actor.id,
        repo.id,
        id,
        Some(level),
    )
    .await
    .map_err(member_error)?;
    Ok(Json(invitation_document(&state, &invitation).await?))
}
async fn delete_invitation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth
        .ok_or_else(crate::backend::api::github::auth::required)?
        .user;
    crate::backend::repositories::collaborators::update_invitation(
        &state.pool,
        actor.id,
        repo.id,
        id,
        None,
    )
    .await
    .map_err(member_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/list-collaborators", get(list)),
        ("repos/check-collaborator", get(check)),
        ("repos/get-collaborator-permission-level", get(permission)),
        ("repos/add-collaborator", put(add)),
        ("repos/remove-collaborator", delete(remove)),
        ("repos/list-invitations", get(invitations)),
        (
            "repos/list-invitations-for-authenticated-user",
            get(invitations),
        ),
        (
            "repos/accept-invitation-for-authenticated-user",
            patch(respond),
        ),
        (
            "repos/decline-invitation-for-authenticated-user",
            delete(respond),
        ),
        ("repos/update-invitation", patch(update_invitation)),
        ("repos/delete-invitation", delete(delete_invitation)),
    ]
}

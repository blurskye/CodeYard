//! Invitations and effective collaborator permissions, including organization and team grants.
use super::{auth, optional_json::OptionalJson, pagination::Pagination, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{self, models::AccessLevel},
    repositories::{
        access,
        collaborators::{self as members, Error as MemberError},
    },
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, put, MethodRouter},
    Json,
};
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

async fn invitation_document(state: &AppState, invitation: &members::Invitation) -> Result<Value> {
    let repo = db::repositories::records::get_repository_by_id(&state.pool, invitation.repo_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let owner = db::repositories::records::repository_owner_name(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let inviter = db::users::accounts::get_user_by_id(&state.pool, invitation.inviter_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let invitee = db::users::accounts::get_user_by_id(&state.pool, invitation.invitee_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(payloads::repository_invitation(
        invitation,
        payloads::repository(&repo, &owner),
        &payloads::Person::of(&inviter),
        &payloads::Person::of(&invitee),
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
    let actor = auth.ok_or_else(auth::required)?.user;
    let affiliation = query.affiliation.as_deref().unwrap_or("all");
    if !matches!(affiliation, "all" | "direct" | "outside") {
        return Err(Error::invalid("Invalid affiliation"));
    }
    let permission = query
        .permission
        .as_deref()
        .map(|name| members::permission(name).ok_or_else(|| Error::invalid("Invalid permission")))
        .transpose()?;
    let (offset, limit) = page.window()?;
    let rows = members::list(
        &state.pool,
        actor.id,
        repo.id,
        &members::CollaboratorQuery {
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
            .map(|row| payloads::collaborator(&row.user, AccessLevel::from(row.permission)))
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
    let actor = auth.ok_or_else(auth::required)?.user;
    let rows = members::list(
        &state.pool,
        actor.id,
        repo.id,
        &members::CollaboratorQuery {
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
    auth.ok_or_else(auth::required)?;
    let user = db::users::accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let level = access::user_access(&state.pool, &user, &repo)
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
    let actor = auth.ok_or_else(auth::required)?.user;
    let input = input.unwrap_or_default();
    let level = members::permission(input.permission.as_deref().unwrap_or("push"))
        .ok_or_else(|| Error::invalid("Invalid permission"))?;
    match members::add(&state.pool, actor.id, repo.id, &username, level)
        .await
        .map_err(member_error)?
    {
        members::Added::Updated => Ok(StatusCode::NO_CONTENT.into_response()),
        members::Added::Invited(invitation) => Ok((
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
    let actor = auth.ok_or_else(auth::required)?.user;
    members::remove(&state.pool, actor.id, repo.id, &username)
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
            (Some(repo.id), auth.ok_or_else(auth::required)?.user)
        } else {
            (
                None,
                auth::authenticate(&state, &headers, true)
                    .await?
                    .ok_or_else(auth::required)?
                    .user,
            )
        };
    let (offset, limit) = page.window()?;
    let rows = members::invitations(&state.pool, actor.id, repo, offset as i64, limit as i64 + 1)
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
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    members::respond(&state.pool, actor.database_actor(), id, method == Method::PATCH)
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
    let actor = auth.ok_or_else(auth::required)?.user;
    let level = members::permission(&input.permissions)
        .ok_or_else(|| Error::invalid("Invalid permission"))?;
    let invitation = members::update_invitation(&state.pool, actor.id, repo.id, id, Some(level))
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
    let actor = auth.ok_or_else(auth::required)?.user;
    members::update_invitation(&state.pool, actor.id, repo.id, id, None)
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

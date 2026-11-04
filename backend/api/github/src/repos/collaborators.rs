//! Invitations and effective collaborator permissions, including organization and team grants.
use axum::response::IntoResponse;
use codeyard_webhooks::payloads;

use crate::optional_json::OptionalJson;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::Method;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_db_core::models::AccessLevel;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_repos::collaborators::Error as MemberError;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

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
    invitation: &codeyard_db_repos::repositories::collaborators::Invitation,
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
    Ok(codeyard_webhooks::payloads::repository_invitation(
        invitation,
        codeyard_webhooks::payloads::repository(&repo, &owner),
        &codeyard_webhooks::payloads::Person::of(&inviter),
        &codeyard_webhooks::payloads::Person::of(&invitee),
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
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
        .user;
    let affiliation = query.affiliation.as_deref().unwrap_or("all");
    if !matches!(affiliation, "all" | "direct" | "outside") {
        return Err(Error::invalid("Invalid affiliation"));
    }
    let permission = query
        .permission
        .as_deref()
        .map(|name| {
            codeyard_repos::collaborators::permission(name)
                .ok_or_else(|| Error::invalid("Invalid permission"))
        })
        .transpose()?;
    let (offset, limit) = page.window()?;
    let rows = codeyard_repos::collaborators::list(
        &state.pool,
        actor.id,
        repo.id,
        &codeyard_db_repos::repositories::collaborators::CollaboratorQuery {
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
                codeyard_webhooks::payloads::collaborator(
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
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
        .user;
    let rows = codeyard_repos::collaborators::list(
        &state.pool,
        actor.id,
        repo.id,
        &codeyard_db_repos::repositories::collaborators::CollaboratorQuery {
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
    auth.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let user = codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let level = codeyard_identity::repository_access::user_access(&state.pool, &user, &repo)
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
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
        .user;
    let input = input.unwrap_or_default();
    let level =
        codeyard_repos::collaborators::permission(input.permission.as_deref().unwrap_or("push"))
            .ok_or_else(|| Error::invalid("Invalid permission"))?;
    match codeyard_repos::collaborators::add(&state.pool, actor.id, repo.id, &username, level)
        .await
        .map_err(member_error)?
    {
        codeyard_db_repos::repositories::collaborators::Added::Updated => {
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        codeyard_db_repos::repositories::collaborators::Added::Invited(invitation) => Ok((
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
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
        .user;
    codeyard_repos::collaborators::remove(&state.pool, actor.id, repo.id, &username)
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
                auth.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
                    .user,
            )
        } else {
            (
                None,
                codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
                    .await?
                    .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
                    .user,
            )
        };
    let (offset, limit) = page.window()?;
    let rows = codeyard_repos::collaborators::invitations(
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
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    actor.require_write()?;
    codeyard_repos::collaborators::respond(
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
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
        .user;
    let level = codeyard_repos::collaborators::permission(&input.permissions)
        .ok_or_else(|| Error::invalid("Invalid permission"))?;
    let invitation = codeyard_repos::collaborators::update_invitation(
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
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
        .user;
    codeyard_repos::collaborators::update_invitation(&state.pool, actor.id, repo.id, id, None)
        .await
        .map_err(member_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
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

use axum::extract::rejection::JsonRejection;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_db_core::models::User;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_db_identity::interactions::InteractionRefusal;
use codeyard_db_identity::interactions::InteractionScope;
use codeyard_db_identity::interactions::NewRestriction;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_support::credentials::AccountRestricted;
use serde_json::json;
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "interactions/get-restrictions-for-authenticated-user",
            get(get_user),
        ),
        (
            "interactions/set-restrictions-for-authenticated-user",
            put(set_user),
        ),
        (
            "interactions/remove-restrictions-for-authenticated-user",
            delete(remove_user),
        ),
        ("interactions/get-restrictions-for-org", get(get_org)),
        ("interactions/set-restrictions-for-org", put(set_org)),
        (
            "interactions/remove-restrictions-for-org",
            delete(remove_org),
        ),
        ("interactions/get-restrictions-for-repo", get(get_repo)),
        ("interactions/set-restrictions-for-repo", put(set_repo)),
        (
            "interactions/remove-restrictions-for-repo",
            delete(remove_repo),
        ),
    ]
}
pub(super) enum ScopeName {
    User,
    Organization(String),
    Repository(String, String),
}
pub(super) async fn resolve(
    state: &AppState,
    actor: &User,
    scope: ScopeName,
) -> Result<InteractionScope> {
    Ok(match scope {
        ScopeName::User => InteractionScope::User(actor.id),
        ScopeName::Organization(name) => InteractionScope::Organization(
            codeyard_db_orgs::organizations::profiles::get_public_organization(&state.pool, &name)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?
                .id,
        ),
        ScopeName::Repository(owner, name) => {
            let repository = codeyard_db_repos::repositories::records::get_repository(
                &state.pool,
                &owner,
                &name,
            )
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
            if !codeyard_identity::repository_access::can_read(
                &state.pool,
                Some(actor),
                &repository,
            )
            .await
            .map_err(Error::unavailable)?
            {
                return Err(Error::missing());
            }
            InteractionScope::Repository(repository.id)
        }
    })
}
fn failure(error: anyhow::Error) -> Error {
    if error.is::<AccountRestricted>()
        || matches!(
            error.downcast_ref::<InteractionRefusal>(),
            Some(InteractionRefusal::Denied)
        )
    {
        Error::new(
            StatusCode::FORBIDDEN,
            "Administration permission is required",
        )
    } else {
        Error::unavailable(error)
    }
}
async fn read(state: Arc<AppState>, headers: HeaderMap, scope: ScopeName) -> Result<Response> {
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let scope = resolve(&state, &actor.user, scope).await?;
    let restriction =
        codeyard_db_identity::interactions::get_restriction(&state.pool, actor.user.id, scope)
            .await
            .map_err(failure)?;
    if restriction.is_none() && matches!(scope, InteractionScope::User(_)) {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(
        Json(restriction.map_or_else(|| json!({}), |restriction| json!(restriction)))
            .into_response(),
    )
}
async fn save(
    state: Arc<AppState>,
    headers: HeaderMap,
    scope: ScopeName,
    input: Option<std::result::Result<Json<NewRestriction>, JsonRejection>>,
) -> Result<Response> {
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    actor.require_write()?;
    let scope = resolve(&state, &actor.user, scope).await?;
    let removing = input.is_none();
    let input = input
        .map(|input| {
            input
                .map(|Json(input)| input)
                .map_err(|_| Error::validation("InteractionLimit", "limit", "invalid"))
        })
        .transpose()?;
    let restriction = match codeyard_db_identity::interactions::set_restriction(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        scope,
        input,
    )
    .await
    {
        Ok(restriction) => restriction,
        Err(error)
            if matches!(
                error.downcast_ref::<InteractionRefusal>(),
                Some(InteractionRefusal::Conflict)
            ) =>
        {
            return Ok(StatusCode::CONFLICT.into_response())
        }
        Err(error) => return Err(failure(error)),
    };
    Ok(if removing {
        StatusCode::NO_CONTENT.into_response()
    } else {
        Json(json!(restriction)).into_response()
    })
}
async fn get_user(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Response> {
    read(state, headers, ScopeName::User).await
}
async fn get_org(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Response> {
    read(state, headers, ScopeName::Organization(org)).await
}
async fn get_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Response> {
    read(state, headers, ScopeName::Repository(owner, name)).await
}
async fn set_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    input: std::result::Result<Json<NewRestriction>, JsonRejection>,
) -> Result<Response> {
    save(state, headers, ScopeName::User, Some(input)).await
}
async fn set_org(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
    input: std::result::Result<Json<NewRestriction>, JsonRejection>,
) -> Result<Response> {
    save(state, headers, ScopeName::Organization(org), Some(input)).await
}
async fn set_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    input: std::result::Result<Json<NewRestriction>, JsonRejection>,
) -> Result<Response> {
    save(
        state,
        headers,
        ScopeName::Repository(owner, name),
        Some(input),
    )
    .await
}
async fn remove_user(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Response> {
    save(state, headers, ScopeName::User, None).await
}
async fn remove_org(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Response> {
    save(state, headers, ScopeName::Organization(org), None).await
}
async fn remove_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Response> {
    save(state, headers, ScopeName::Repository(owner, name), None).await
}

pub mod pull_caps;

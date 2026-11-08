use crate::interactions::ScopeName;
use axum::extract::rejection::JsonRejection;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_db_identity::interactions::InteractionScope;
use codeyard_db_repos::pull_caps::CapUpdate;
use codeyard_db_repos::pull_caps::PullCap;
use codeyard_db_repos::pull_caps::PullCapRefusal;
use codeyard_db_repos::pull_caps::PullCapScope;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_types::credentials::AccountRestricted;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "interactions/get-pull-request-creation-cap-for-org",
            get(get_org),
        ),
        (
            "interactions/update-pull-request-creation-cap-for-org",
            patch(update_org),
        ),
        (
            "interactions/get-pull-request-creation-cap-for-repo",
            get(get_repo),
        ),
        (
            "interactions/update-pull-request-creation-cap-for-repo",
            patch(update_repo),
        ),
        (
            "interactions/get-pull-request-bypass-list-for-repo",
            get(bypass),
        ),
        (
            "interactions/set-pull-request-bypass-list-for-repo",
            put(add_bypass),
        ),
        (
            "interactions/remove-pull-request-bypass-list-for-repo",
            delete(remove_bypass),
        ),
    ]
}
fn failure(error: anyhow::Error) -> Error {
    match error.downcast_ref::<PullCapRefusal>() {
        Some(PullCapRefusal::Invalid) => Error::validation(
            "PullRequestCreationCap",
            "max_open_pull_requests",
            "invalid",
        ),
        Some(PullCapRefusal::Limit | PullCapRefusal::MissingUser) => {
            Error::validation("PullRequestBypassList", "users", "invalid")
        }
        Some(PullCapRefusal::Denied) => Error::new(
            StatusCode::FORBIDDEN,
            "Administration permission is required",
        ),
        None if error.is::<AccountRestricted>() => Error::new(
            StatusCode::FORBIDDEN,
            "Account cannot change interaction settings",
        ),
        None => Error::unavailable(error),
    }
}
async fn actor_scope(
    state: &AppState,
    headers: &HeaderMap,
    scope: ScopeName,
    write: bool,
) -> Result<(codeyard_identity::github_auth::Authentication, PullCapScope)> {
    let actor = codeyard_identity::github_auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    if write {
        actor.require_write()?;
    }
    let scope = match crate::interactions::resolve(state, &actor.user, scope).await? {
        InteractionScope::Organization(id) => PullCapScope::Organization(id),
        InteractionScope::Repository(id) => PullCapScope::Repository(id),
        InteractionScope::User(_) => return Err(Error::invalid("Invalid cap scope")),
    };
    Ok((actor, scope))
}
async fn read(state: Arc<AppState>, headers: HeaderMap, scope: ScopeName) -> Result<Json<PullCap>> {
    let (actor, scope) = actor_scope(&state, &headers, scope, false).await?;
    Ok(Json(
        codeyard_db_repos::pull_caps::get_cap(&state.pool, actor.user.id, scope)
            .await
            .map_err(failure)?,
    ))
}
async fn update(
    state: Arc<AppState>,
    headers: HeaderMap,
    scope: ScopeName,
    input: std::result::Result<Json<CapUpdate>, JsonRejection>,
) -> Result<Json<PullCap>> {
    let (actor, scope) = actor_scope(&state, &headers, scope, true).await?;
    let Json(input) = input.map_err(codeyard_model::github_error::json_error)?;
    Ok(Json(
        codeyard_db_repos::pull_caps::update_cap(
            &state.pool,
            actor.user.id,
            &AccountCredential::Token(actor.token.id, &actor.token.token_hash),
            scope,
            &input,
        )
        .await
        .map_err(failure)?,
    ))
}
async fn get_org(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<PullCap>> {
    read(state, headers, ScopeName::Organization(org)).await
}
async fn get_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<PullCap>> {
    read(state, headers, ScopeName::Repository(owner, repo)).await
}
async fn update_org(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
    input: std::result::Result<Json<CapUpdate>, JsonRejection>,
) -> Result<Json<PullCap>> {
    update(state, headers, ScopeName::Organization(org), input).await
}
async fn update_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    input: std::result::Result<Json<CapUpdate>, JsonRejection>,
) -> Result<Json<PullCap>> {
    update(state, headers, ScopeName::Repository(owner, repo), input).await
}
async fn bypass(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<Vec<Value>>> {
    let (actor, scope) =
        actor_scope(&state, &headers, ScopeName::Repository(owner, repo), false).await?;
    let users = codeyard_db_repos::pull_caps::bypass_users(&state.pool, actor.user.id, scope.id())
        .await
        .map_err(failure)?;
    Ok(Json(
        users
            .iter()
            .map(codeyard_webhooks::payloads::user)
            .collect(),
    ))
}
#[derive(Deserialize)]
struct Bypass {
    users: Vec<String>,
}
async fn change_bypass(
    state: Arc<AppState>,
    headers: HeaderMap,
    owner: String,
    repo: String,
    input: std::result::Result<Json<Bypass>, JsonRejection>,
    adding: bool,
) -> Result<StatusCode> {
    let (actor, scope) =
        actor_scope(&state, &headers, ScopeName::Repository(owner, repo), true).await?;
    let Json(input) = input.map_err(codeyard_model::github_error::json_error)?;
    codeyard_db_repos::pull_caps::change_bypass(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        scope.id(),
        &input.users,
        adding,
    )
    .await
    .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn add_bypass(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    input: std::result::Result<Json<Bypass>, JsonRejection>,
) -> Result<StatusCode> {
    change_bypass(state, headers, owner, repo, input, true).await
}
async fn remove_bypass(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    input: std::result::Result<Json<Bypass>, JsonRejection>,
) -> Result<StatusCode> {
    change_bypass(state, headers, owner, repo, input, false).await
}

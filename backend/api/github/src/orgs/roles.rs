//! Predefined roles inherit native repository permissions without granting organization ownership.
use crate::orgs::outcome;
use crate::orgs::target;
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
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_orgs::organizations::roles::OrganizationRole;
use codeyard_db_orgs::organizations::roles::RoleTarget;
use codeyard_db_repos::repositories::push_teams::TeamAccess;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

type Params = HashMap<String, String>;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("orgs/list-org-roles", get(list)),
        ("orgs/get-org-role", get(get_role)),
        ("orgs/list-org-role-users", get(users)),
        ("orgs/list-org-role-teams", get(teams)),
        ("orgs/assign-user-to-org-role", put(change)),
        ("orgs/assign-team-to-org-role", put(change)),
        ("orgs/revoke-org-role-user", delete(change)),
        ("orgs/revoke-org-role-team", delete(change)),
        ("orgs/revoke-all-org-roles-user", delete(change)),
        ("orgs/revoke-all-org-roles-team", delete(change)),
    ]
}
async fn required_role(
    state: &AppState,
    actor: i64,
    org: i64,
    id: i64,
) -> Result<OrganizationRole> {
    outcome(
        codeyard_db_orgs::organizations::roles::catalog(&state.pool, actor, org)
            .await
            .map_err(Error::unavailable)?,
    )?
    .into_iter()
    .find(|role| role.id == id)
    .ok_or_else(Error::missing)
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    let org = target(&state, &name).await?;
    let roles = outcome(
        codeyard_db_orgs::organizations::roles::catalog(&state.pool, actor.user.id, org.id)
            .await
            .map_err(Error::unavailable)?,
    )?;
    Ok(Json(
        json!({"total_count":roles.len(),"roles":roles.iter().map(codeyard_webhooks::payloads::organization_role).collect::<Vec<_>>()}),
    ))
}
async fn get_role(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
) -> Result<Json<Value>> {
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    let org = target(&state, &name).await?;
    let role = required_role(&state, actor.user.id, org.id, id).await?;
    Ok(Json(codeyard_webhooks::payloads::organization_role(&role)))
}
async fn users(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    let org = target(&state, &name).await?;
    let role = required_role(&state, actor.user.id, org.id, id).await?;
    let (offset, limit) = page.window()?;
    let mut users = outcome(
        codeyard_db_orgs::organizations::roles::users(
            &state.pool,
            actor.user.id,
            org.id,
            id,
            0,
            (offset as i64, (limit + 1) as i64),
        )
        .await
        .map_err(Error::unavailable)?,
    )?;
    let more = users.len() > limit;
    users.truncate(limit);
    page.response(
        &uri,
        users
            .iter()
            .map(|user| {
                codeyard_webhooks::payloads::organization_role_user(
                    user,
                    &org.name,
                    role.base_level,
                )
            })
            .collect(),
        more,
    )
}
async fn teams(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    let org = target(&state, &name).await?;
    let role = required_role(&state, actor.user.id, org.id, id).await?;
    let (offset, limit) = page.window()?;
    let mut teams = outcome(
        codeyard_db_orgs::organizations::roles::teams(
            &state.pool,
            actor.user.id,
            org.id,
            id,
            0,
            (offset as i64, (limit + 1) as i64),
        )
        .await
        .map_err(Error::unavailable)?,
    )?;
    let more = teams.len() > limit;
    teams.truncate(limit);
    page.response(
        &uri,
        teams
            .into_iter()
            .map(|team| {
                let mut value = codeyard_webhooks::payloads::team(&TeamAccess {
                    team,
                    org_slug: org.name.clone(),
                    permission: role.base_level,
                });
                value["assignment"] = json!("direct");
                value
            })
            .collect(),
        more,
    )
}
async fn change(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    method: Method,
) -> Result<StatusCode> {
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    actor.require_write()?;
    let org = target(&state, &params["org"]).await?;
    let role = params
        .get("role_id")
        .map(|id| id.parse::<i64>().map_err(|_| Error::missing()))
        .transpose()?;
    let target = if let Some(name) = params.get("username") {
        RoleTarget::User(name.clone())
    } else {
        RoleTarget::Team(params["team_slug"].clone())
    };
    let result = codeyard_db_orgs::organizations::roles::change(
        &state.pool,
        actor.database_actor(),
        org.id,
        &target,
        role,
        method == Method::PUT,
    )
    .await
    .map_err(Error::unavailable)?;
    if matches!(
        result,
        codeyard_database::queries::organizations::OrganizationOutcome::Conflict
    ) {
        return Err(Error::invalid(
            "Only organization members can be assigned organization roles",
        ));
    }
    outcome(result)?;
    Ok(StatusCode::NO_CONTENT)
}

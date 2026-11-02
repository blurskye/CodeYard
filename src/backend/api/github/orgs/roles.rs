//! Predefined roles inherit native repository permissions without granting organization ownership.
use super::{auth, outcome, target, AppState, Error, Result};
use crate::backend::{
    api::github::pagination::Pagination,
    db::{
        organizations::roles::{self, OrganizationRole, RoleTarget},
        repositories::push_teams::TeamAccess,
    },
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
    routing::{delete, get, put, MethodRouter},
    Json,
};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
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
        roles::catalog(&state.pool, actor, org)
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
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let roles = outcome(
        roles::catalog(&state.pool, actor.user.id, org.id)
            .await
            .map_err(Error::unavailable)?,
    )?;
    Ok(Json(
        json!({"total_count":roles.len(),"roles":roles.iter().map(payloads::organization_role).collect::<Vec<_>>()}),
    ))
}
async fn get_role(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let role = required_role(&state, actor.user.id, org.id, id).await?;
    Ok(Json(payloads::organization_role(&role)))
}
async fn users(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let role = required_role(&state, actor.user.id, org.id, id).await?;
    let (offset, limit) = page.window()?;
    let mut users = outcome(
        roles::users(
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
            .map(|user| payloads::organization_role_user(user, &org.name, role.base_level))
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
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let role = required_role(&state, actor.user.id, org.id, id).await?;
    let (offset, limit) = page.window()?;
    let mut teams = outcome(
        roles::teams(
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
                let mut value = payloads::team(&TeamAccess {
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
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
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
    let result = roles::change(
        &state.pool,
        actor.user.id,
        org.id,
        &target,
        role,
        method == Method::PUT,
    )
    .await
    .map_err(Error::unavailable)?;
    if matches!(
        result,
        crate::backend::db::organizations::OrganizationOutcome::Conflict
    ) {
        return Err(Error::invalid(
            "Only organization members can be assigned organization roles",
        ));
    }
    outcome(result)?;
    Ok(StatusCode::NO_CONTENT)
}

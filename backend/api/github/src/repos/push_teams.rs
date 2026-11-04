//! Native organization-team push allowances, including live membership enforcement.
use crate::auth::read_method;
use crate::repos::protection::protected;
use crate::repos::protection::write_error;
use axum::extract::Path;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::Method;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_db_core::models::BranchProtectionRule;
use codeyard_db_repos::repositories::protection::ActorListUpdate;
use codeyard_db_repos::repositories::protection::ProtectionUpdate;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

pub(super) async fn documents(
    state: &AppState,
    rule: &BranchProtectionRule,
    actor: i64,
) -> Result<Vec<Value>> {
    let rows = codeyard_db_repos::repositories::push_teams::by_ids(
        &state.pool,
        rule.repo_id,
        &rule.push_team_allowances,
    )
    .await
    .map_err(Error::unavailable)?;
    let mut values = Vec::new();
    for row in rows {
        if super::get_team_by_id(&state.pool, actor, row.team.id)
            .await
            .map_err(Error::unavailable)?
            .is_none()
        {
            continue;
        }
        values.push(crate::teams::summary(state, &row.team, &row.org_slug, row.permission).await?);
    }
    Ok(values)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Input {
    Array(Vec<String>),
    Object { teams: Vec<String> },
}

async fn teams(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
    method: Method,
    request: Request,
) -> Result<Response> {
    let (mut rule, actor, _) = protected(
        &state,
        &headers,
        &owner,
        &name,
        &branch,
        !read_method(&method),
    )
    .await?;
    if !rule.restrict_pushes {
        return Err(Error::missing());
    }
    if !read_method(&method) {
        let Json(input) = Json::<Input>::from_request(request, &state)
            .await
            .map_err(codeyard_model::github_error::json_error)?;
        let (Input::Array(names) | Input::Object { teams: names }) = input;
        let ids = codeyard_db_repos::repositories::push_teams::resolve_names(
            &state.pool,
            rule.repo_id,
            &names,
        )
        .await
        .map_err(write_error)?;
        let update = match method {
            Method::POST => ActorListUpdate::Add(ids),
            Method::PUT => ActorListUpdate::Set(ids),
            Method::DELETE => ActorListUpdate::Remove(ids),
            _ => unreachable!(),
        };
        rule = codeyard_db_repos::repositories::protection::update_branch_protection_rule(
            &state.pool,
            actor.clone(),
            rule.repo_id,
            &rule.pattern,
            ProtectionUpdate::PushTeams(update),
        )
        .await
        .map_err(write_error)?;
    }
    Ok(Json(documents(&state, &rule, actor.user_id).await?).into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "repos/get-teams-with-access-to-protected-branch",
            get(teams),
        ),
        ("repos/add-team-access-restrictions", post(teams)),
        ("repos/set-team-access-restrictions", put(teams)),
        ("repos/remove-team-access-restrictions", delete(teams)),
    ]
}

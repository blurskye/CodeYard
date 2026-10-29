//! Native organization-team push allowances, including live membership enforcement.
use super::{
    auth::read_method,
    protection::{protected, write_error},
    Error, Result,
};
use crate::backend::{
    app::AppState,
    db::{
        self,
        models::BranchProtectionRule,
        repositories::protection::{ActorListUpdate, ProtectionUpdate},
    },
};
use axum::{
    extract::{FromRequest, Path, Request, State},
    http::{HeaderMap, Method},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

pub(super) async fn documents(state: &AppState, rule: &BranchProtectionRule) -> Result<Vec<Value>> {
    let rows =
        db::repositories::push_teams::by_ids(&state.pool, rule.repo_id, &rule.push_team_allowances)
            .await
            .map_err(Error::unavailable)?;
    let mut values = Vec::new();
    for row in rows {
        values.push(super::teams::summary(state, &row.team, &row.org_slug, row.permission).await?);
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
            .map_err(|_| Error::invalid("Expected an array of team slugs"))?;
        let (Input::Array(names) | Input::Object { teams: names }) = input;
        let ids = db::repositories::push_teams::resolve_names(&state.pool, rule.repo_id, &names)
            .await
            .map_err(write_error)?;
        let update = match method {
            Method::POST => ActorListUpdate::Add(ids),
            Method::PUT => ActorListUpdate::Set(ids),
            Method::DELETE => ActorListUpdate::Remove(ids),
            _ => unreachable!(),
        };
        rule = db::repositories::protection::update_branch_protection_rule(
            &state.pool,
            actor,
            rule.repo_id,
            &rule.pattern,
            ProtectionUpdate::PushTeams(update),
        )
        .await
        .map_err(write_error)?;
    }
    Ok(Json(documents(&state, &rule).await?).into_response())
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

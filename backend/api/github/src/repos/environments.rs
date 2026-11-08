//! Environment configuration is shared with Actions rather than kept in API-only records.
use axum::response::IntoResponse;

use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::Request;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::Method;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_db_actions::environments::Branch;
use codeyard_db_actions::environments::Environment;
use codeyard_db_actions::environments::Refusal;
use codeyard_db_actions::environments::Reviewer;
use codeyard_db_actions::environments::Update;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use codeyard_types::urls::repo_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

type Params = HashMap<String, String>;

pub(super) fn write_error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Environment administration denied")
        }
        Some(Refusal::NotFound) => Error::missing(),
        Some(Refusal::Invalid | Refusal::Duplicate) => {
            Error::validation("Environment", "configuration", "invalid")
        }
        None => Error::unavailable(error),
    }
}

/// `viewer` is the user reading the environment. A secret reviewer team is shown only when that
/// user can see the team, as team reads decide.
pub(crate) async fn document(
    state: &AppState,
    viewer: Option<i64>,
    env: &Environment,
    owner: &str,
    repo: &str,
) -> Result<Value> {
    let selected: Vec<Reviewer> =
        serde_json::from_str(&env.reviewers).map_err(Error::unavailable)?;
    let mut reviewers = Vec::new();
    for reviewer in selected {
        let value = if reviewer.kind == "User" {
            codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, reviewer.id)
                .await
                .map_err(Error::unavailable)?
                .map(|user| codeyard_webhooks::payloads::user(&user))
        } else {
            let team = codeyard_db_repos::repositories::push_teams::by_ids(
                &state.pool,
                env.repo_id,
                &[reviewer.id],
            )
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .next();
            match team {
                Some(team) if team.team.privacy == "visible" => {
                    Some(codeyard_webhooks::payloads::team(&team))
                }
                Some(team) => {
                    let can_see = match viewer {
                        Some(viewer) => codeyard_db_orgs::organizations::teams::get_team_by_id(
                            &state.pool,
                            viewer,
                            team.team.id,
                        )
                        .await
                        .map_err(Error::unavailable)?
                        .is_some(),
                        None => false,
                    };
                    can_see.then(|| codeyard_webhooks::payloads::team(&team))
                }
                None => None,
            }
        };
        if let Some(value) = value {
            reviewers.push(json!({"type": reviewer.kind, "reviewer": value}));
        }
    }
    Ok(codeyard_webhooks::payloads::environment(
        env, owner, repo, reviewers,
    ))
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let (offset, limit) = page.window()?;
    let rows = codeyard_db_actions::environments::list(
        &state.pool,
        repo.id,
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = rows.len() > limit;
    let mut values = Vec::new();
    for row in rows.into_iter().take(limit) {
        values.push(
            document(
                &state,
                auth.as_ref().map(|auth| auth.user.id),
                &row,
                &owner,
                &name,
            )
            .await?,
        );
    }
    let total = codeyard_db_actions::environments::count(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?;
    Ok((
        page.headers(&uri, has_next)?,
        Json(json!({"total_count": total, "environments": values})),
    )
        .into_response())
}

async fn administer(
    state: &AppState,
    headers: &HeaderMap,
    params: &Params,
    write: bool,
) -> Result<(
    codeyard_database::models::Repository,
    codeyard_identity::github_auth::Authentication,
)> {
    let (repo, auth) = repository(state, headers, &params["owner"], &params["repo"], write).await?;
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    if !codeyard_identity::repository_access::can_admin(&state.pool, &auth.user, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::missing());
    }
    Ok((repo, auth))
}

async fn environment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    request: Request,
) -> Result<Response> {
    let method = request.method().clone();
    let name = &params["environment_name"];
    let (env, viewer) = if crate::auth::read_method(&method) {
        let (repo, auth) =
            repository(&state, &headers, &params["owner"], &params["repo"], false).await?;
        let env = codeyard_db_actions::environments::get(&state.pool, repo.id, name)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        (env, auth.map(|auth| auth.user.id))
    } else {
        let (repo, auth) = administer(&state, &headers, &params, true).await?;
        if method == Method::DELETE {
            codeyard_db_actions::environments::delete(
                &state.pool,
                auth.database_actor(),
                repo.id,
                name,
            )
            .await
            .map_err(write_error)?;
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        let bytes = axum::body::to_bytes(request.into_body(), 65536)
            .await
            .map_err(|_| Error::invalid("Invalid environment configuration"))?;
        let input: Update = if bytes.is_empty() {
            Update::default()
        } else {
            serde_json::from_slice::<Option<Update>>(&bytes)
                .map_err(|_| Error::validation("Environment", "configuration", "invalid"))?
                .unwrap_or_default()
        };
        let env = codeyard_db_actions::environments::upsert(
            &state.pool,
            auth.database_actor(),
            repo.id,
            name,
            &input,
        )
        .await
        .map_err(write_error)?;
        (env, Some(auth.user.id))
    };
    Ok(
        Json(document(&state, viewer, &env, &params["owner"], &params["repo"]).await?)
            .into_response(),
    )
}

fn branch_document(branch: &Branch, repo: i64) -> Value {
    json!({"id": branch.id, "node_id": codeyard_types::node_id::scoped("GBP", repo, branch.id), "name": branch.name, "type": branch.kind})
}

#[derive(Deserialize)]
struct BranchInput {
    name: String,
    #[serde(rename = "type")]
    kind: Option<String>,
}

async fn policies(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Result<Response> {
    let method = request.method().clone();
    let read = crate::auth::read_method(&method);
    let (repo, auth) = if read {
        let (repo, _) =
            repository(&state, &headers, &params["owner"], &params["repo"], false).await?;
        (repo, None)
    } else {
        let (repo, auth) = administer(&state, &headers, &params, true).await?;
        (repo, Some(auth))
    };
    let env =
        codeyard_db_actions::environments::get(&state.pool, repo.id, &params["environment_name"])
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    let id = params
        .get("branch_policy_id")
        .map(|id| id.parse::<i64>().map_err(|_| Error::missing()))
        .transpose()?;
    if read {
        if let Some(id) = id {
            let branch = codeyard_db_actions::environments::branch(&state.pool, env.id, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            return Ok(Json(branch_document(&branch, repo.id)).into_response());
        }
        let (offset, limit) = page.window()?;
        let rows = codeyard_db_actions::environments::branches(
            &state.pool,
            env.id,
            offset as i64,
            (limit + 1) as i64,
        )
        .await
        .map_err(Error::unavailable)?;
        let total = codeyard_db_actions::environments::branch_count(&state.pool, env.id)
            .await
            .map_err(Error::unavailable)?;
        return Ok((page.headers(&uri, rows.len() > limit)?, Json(json!({"total_count": total, "branch_policies": rows.iter().take(limit).map(|branch| branch_document(branch, repo.id)).collect::<Vec<_>>()}))).into_response());
    }
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    if method == Method::DELETE {
        codeyard_db_actions::environments::delete_branch(
            &state.pool,
            auth.database_actor(),
            repo.id,
            env.id,
            id.ok_or_else(Error::missing)?,
        )
        .await
        .map_err(write_error)?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let bytes = axum::body::to_bytes(request.into_body(), 65536)
        .await
        .map_err(|_| Error::invalid("Invalid branch policy"))?;
    let input: BranchInput = serde_json::from_slice(&bytes)
        .map_err(|_| Error::validation("DeploymentBranchPolicy", "name", "invalid"))?;
    let kind = if let Some(id) = id {
        codeyard_db_actions::environments::branch(&state.pool, env.id, id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?
            .kind
    } else {
        input.kind.unwrap_or_else(|| "branch".into())
    };
    let (branch, duplicate) = codeyard_db_actions::environments::save_branch(
        &state.pool,
        auth.database_actor(),
        repo.id,
        env.id,
        id,
        &input.name,
        &kind,
    )
    .await
    .map_err(write_error)?;
    if duplicate {
        return Ok((
            StatusCode::SEE_OTHER,
            [(
                header::LOCATION,
                format!(
                    "{}/environments/{}/deployment-branch-policies/{}",
                    repo_url(&params["owner"], &params["repo"]),
                    codeyard_types::paths::encode_component(&env.name),
                    branch.id
                ),
            )],
        )
            .into_response());
    }
    Ok(Json(branch_document(&branch, repo.id)).into_response())
}

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/get-all-environments", get(list)),
        ("repos/get-environment", get(environment)),
        (
            "repos/create-or-update-environment",
            axum::routing::put(environment),
        ),
        (
            "repos/delete-an-environment",
            axum::routing::delete(environment),
        ),
        ("repos/list-deployment-branch-policies", get(policies)),
        ("repos/get-deployment-branch-policy", get(policies)),
        (
            "repos/create-deployment-branch-policy",
            axum::routing::post(policies),
        ),
        (
            "repos/update-deployment-branch-policy",
            axum::routing::put(policies),
        ),
        (
            "repos/delete-deployment-branch-policy",
            axum::routing::delete(policies),
        ),
    ]
}

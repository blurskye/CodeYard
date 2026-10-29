//! Custom environment rules use installed GitHub Apps and the same gates as Actions.
use super::{api_url, auth, node_id, pagination::Pagination, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        deployment_protection::{self as protection, Refusal, Rule},
        environments,
    },
    repositories::access,
};
use axum::{
    extract::{OriginalUri, Path, Query, Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

type Params = HashMap<String, String>;

pub(super) fn write_error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => Error::new(
            StatusCode::FORBIDDEN,
            "Resource not accessible by integration",
        ),
        Some(Refusal::NotFound) => Error::missing(),
        Some(Refusal::Invalid) => {
            Error::validation("DeploymentProtectionRule", "integration_id", "invalid")
        }
        Some(Refusal::Conflict) => Error::new(
            StatusCode::CONFLICT,
            "Deployment protection decision conflicts with the current state",
        ),
        None => Error::unavailable(error),
    }
}

fn integration(id: i64, slug: &str) -> Value {
    json!({"id":id,"slug":slug,"integration_url":api_url(&format!("/apps/{}", crate::webui::encode_component(slug))),"node_id":node_id::encode("APP", id)})
}
fn document(rule: &Rule) -> Value {
    json!({"id":rule.id,"node_id":node_id::encode("DPR",rule.id),"enabled":rule.enabled,"app":integration(rule.app_id,&rule.slug)})
}

#[derive(Deserialize)]
struct Input {
    integration_id: i64,
}

async fn rules(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Result<Response> {
    let read = auth::read_method(request.method());
    let (repo, authentication) =
        repository(&state, &headers, &params["owner"], &params["repo"], !read).await?;
    if !read {
        let actor = &authentication.as_ref().ok_or_else(auth::required)?.user;
        if !access::can_admin(&state.pool, actor, &repo)
            .await
            .map_err(Error::unavailable)?
        {
            return Err(Error::missing());
        }
    }
    let env = environments::get(&state.pool, repo.id, &params["environment_name"])
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let id = params
        .get("protection_rule_id")
        .map(|id| id.parse::<i64>().map_err(|_| Error::missing()))
        .transpose()?;
    if read {
        let rules = protection::rules(&state.pool, env.id)
            .await
            .map_err(Error::unavailable)?;
        if let Some(id) = id {
            let rule = rules
                .iter()
                .find(|rule| rule.id == id)
                .ok_or_else(Error::missing)?;
            return Ok(Json(document(rule)).into_response());
        }
        let (offset, limit) = page.window()?;
        return Ok((page.headers(&uri, rules.len() > offset + limit)?, Json(json!({"total_count":rules.len(),"custom_deployment_protection_rules":rules.iter().skip(offset).take(limit).map(document).collect::<Vec<_>>()}))).into_response());
    }
    let authentication = authentication.ok_or_else(auth::required)?;
    let actor = (authentication.user.id, authentication.token.id);
    if request.method() == Method::DELETE {
        protection::disable(
            &state.pool,
            actor,
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
        .map_err(|_| Error::invalid("Invalid deployment protection rule"))?;
    let input: Input = serde_json::from_slice(&bytes)
        .map_err(|_| Error::validation("DeploymentProtectionRule", "integration_id", "invalid"))?;
    let rule = protection::create(&state.pool, actor, repo.id, env.id, input.integration_id)
        .await
        .map_err(write_error)?;
    Ok((StatusCode::CREATED, Json(document(&rule))).into_response())
}

async fn integrations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, authentication) =
        repository(&state, &headers, &params["owner"], &params["repo"], false).await?;
    let actor = authentication.ok_or_else(auth::required)?.user;
    if !access::can_admin(&state.pool, &actor, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::missing());
    }
    environments::get(&state.pool, repo.id, &params["environment_name"])
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let apps = protection::integrations(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?;
    let (offset, limit) = page.window()?;
    Ok((page.headers(&uri, apps.len() > offset + limit)?, Json(json!({"total_count":apps.len(),"available_custom_deployment_protection_rule_integrations":apps.iter().skip(offset).take(limit).map(|app| integration(app.id,&app.slug)).collect::<Vec<_>>()}))).into_response())
}

#[derive(Deserialize)]
struct GateInput {
    environment_name: String,
    state: Option<String>,
    comment: Option<String>,
}

/// The App behind an installation token decides its own gate; the database checks the
/// credential, its permissions and the run's latest attempt.
async fn review_gate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Json(input): Json<GateInput>,
) -> Result<StatusCode> {
    let (repo, authentication) =
        repository(&state, &headers, &params["owner"], &params["repo"], false).await?;
    let token = authentication.ok_or_else(auth::required)?.token.id;
    let run = params["run_id"]
        .parse::<i64>()
        .map_err(|_| Error::missing())?;
    protection::review(
        &state.pool,
        token,
        repo.id,
        run,
        &protection::Review {
            environment_name: &input.environment_name,
            state: input.state.as_deref(),
            comment: input.comment.as_deref(),
        },
    )
    .await
    .map_err(write_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/review-custom-gates-for-run", post(review_gate)),
        ("repos/get-all-deployment-protection-rules", get(rules)),
        ("repos/create-deployment-protection-rule", post(rules)),
        (
            "repos/list-custom-deployment-rule-integrations",
            get(integrations),
        ),
        ("repos/get-custom-deployment-protection-rule", get(rules)),
        ("repos/disable-deployment-protection-rule", delete(rules)),
    ]
}

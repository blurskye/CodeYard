use super::super::{auth, repository, Error, Result};
use super::{document, target};
use crate::backend::{
    app::AppState,
    db,
    repositories::{
        access,
        pull_requests::{self, mutations},
    },
};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{patch, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

pub(in crate::backend::api::github) fn operation_error(error: pull_requests::OperationError) -> Error {
    use pull_requests::OperationError::*;
    match error {
        Missing { .. } => Error::missing(),
        Unauthenticated { .. } => auth::required(),
        Denied { message, .. } => Error::new(StatusCode::FORBIDDEN, message),
        Invalid { message, .. } => Error::invalid(message),
        Conflict { message, .. } => Error::new(StatusCode::CONFLICT, message),
        TooLarge { message, .. } => Error::new(StatusCode::PAYLOAD_TOO_LARGE, message),
        _ => Error::unavailable(()),
    }
}

#[derive(Deserialize)]
struct Create {
    title: Option<String>,
    body: Option<String>,
    head: String,
    base: String,
    head_repo: Option<String>,
    draft: Option<bool>,
    issue: Option<i64>,
    maintainer_can_modify: Option<bool>,
}

async fn create(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    namespace: Path<(String, String)>,
    body: Bytes,
) -> Result<Response> {
    match create_value(state, headers, namespace, body).await {
        Err(Error(StatusCode::NOT_FOUND, _, _)) => Ok(StatusCode::NOT_FOUND.into_response()),
        result => result,
    }
}

async fn create_value(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    body: Bytes,
) -> Result<Response> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let viewer = viewer.ok_or_else(auth::required)?;
    let actor = viewer.user.clone();
    let input: Create =
        serde_json::from_slice(&body).map_err(super::super::json_parse_error)?;
    let source = match input.issue {
        Some(number) => mutations::PullRequestSource::Issue {
            issue: i32::try_from(number)
                .ok()
                .filter(|number| *number > 0)
                .ok_or_else(|| Error::invalid("issue must be a positive discussion number"))?,
        },
        None => mutations::PullRequestSource::New {
            title: input
                .title
                .ok_or_else(|| Error::invalid("title is required"))?,
            body: input.body,
        },
    };
    let (head_owner, branch) = input.head.split_once(':').unwrap_or((&owner, &input.head));
    let (head_owner, head_name) = match input.head_repo.as_deref() {
        Some(nwo) => nwo.split_once('/').unwrap_or((head_owner, nwo)),
        None => (head_owner, name.as_str()),
    };
    let head = if input.head_repo.is_some() {
        db::repositories::records::get_repository(&state.pool, head_owner, head_name).await
    } else {
        pull_requests::branches::owner_head_repository(&state.pool, &repo, head_owner).await
    }
    .map_err(Error::unavailable)?
    .ok_or_else(|| Error::invalid("Invalid head repository"))?;
    if !access::can_read(&state.pool, Some(&actor), &head)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::missing());
    }
    // The head's commits become readable through this repository, so a credential bounded
    // to some repositories must reach the head repository as well.
    viewer.repository_scope(&state.pool, head.id, "contents", false)
        .await.map_err(auth::scope_error)?;
    let pull = mutations::create_pull_request(
        &state.pool,
        &state.ci_runs,
        &state.notification_broadcaster,
        crate::backend::repositories::store::PullHeadActor { user: actor.id, token: Some(viewer.token.id) },
        owner.clone(),
        name,
        mutations::CreatePullRequestRequest {
            suppress_ci: false,
            head_repo_id: Some(head.id),
            source,
            head_branch: branch.to_owned(),
            base_branch: input.base,
            is_draft: input.draft.unwrap_or(false),
            maintainer_can_modify: Some(input.maintainer_can_modify.unwrap_or_else(|| {
                db::pull_requests::records::can_grant_maintainer_edit(actor.id, repo.id, &head)
            })),
        },
    )
    .await
    .map_err(operation_error)?;
    let value = document(&state, &headers, &repo, &owner, &pull, true).await?;
    let location = value["url"]
        .as_str()
        .ok_or_else(|| Error::unavailable(()))?
        .to_owned();
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(value),
    )
        .into_response())
}

#[derive(Default, Deserialize)]
struct Update {
    title: Option<String>,
    body: Option<String>,
    state: Option<String>,
    base: Option<String>,
    maintainer_can_modify: Option<bool>,
}

async fn update(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    namespace: Path<(String, String, i32)>,
    body: Bytes,
) -> Result<Response> {
    match update_value(state, headers, namespace, body).await {
        Ok(value) => Ok(value.into_response()),
        Err(Error(StatusCode::NOT_FOUND, _, _)) => Ok(StatusCode::NOT_FOUND.into_response()),
        Err(error) => Err(error),
    }
}

async fn update_value(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<Json<Value>> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = viewer.ok_or_else(auth::required)?.user;
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    if pull.head_repo_id.is_some()
        && super::readable_head(&state, &headers, &pull)
            .await?
            .is_none()
    {
        return Err(Error::missing());
    }
    let input: Update = if body.is_empty() {
        Update::default()
    } else {
        serde_json::from_slice(&body).map_err(super::super::json_parse_error)?
    };
    let pull = mutations::update_pull_request_latest(
        &state.pool,
        &state.ci_runs,
        &state.notification_broadcaster,
        actor.id,
        (repo.id, number),
        mutations::UpdatePullRequestRequest {
            expected_state: pull.state,
            expected_updated: pull.updated_at.to_rfc3339(),
            title: input.title,
            body: input.body,
            state: input.state,
            base: input.base,
            maintainer_can_modify: input.maintainer_can_modify,
        },
    )
    .await
    .map_err(operation_error)?;
    Ok(Json(
        document(&state, &headers, &repo, &owner, &pull, true).await?,
    ))
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("pulls/create", post(create)),
        ("pulls/update", patch(update)),
    ]
}

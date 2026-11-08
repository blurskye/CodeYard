use crate::pulls::target;
use axum::body::Bytes;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_git_storage::store_types::MergeMethod;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

#[derive(Default, Deserialize)]
struct Merge {
    sha: Option<String>,
    merge_method: Option<String>,
    commit_title: Option<String>,
    commit_message: Option<String>,
}

fn operation_error(
    error: codeyard_git_storage::repositories::pull_requests::OperationError,
) -> Error {
    use codeyard_git_storage::repositories::pull_requests::OperationError;
    match error {
        OperationError::Conflict {
            code: "revision_conflict",
            message,
        } => Error::new(StatusCode::CONFLICT, message),
        OperationError::Conflict { message, .. }
        | OperationError::Invalid {
            code: "invalid_state",
            message,
        }
        | OperationError::Denied {
            code: "branch_protection",
            message,
        } => Error::new(StatusCode::METHOD_NOT_ALLOWED, message),
        error => crate::pulls::mutations::operation_error(error),
    }
}

async fn merge_pull(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    // The merge moves the base branch, so the credential needs contents write as well as the
    // pull request grant the route asked for.
    auth.repository_scope(&state.pool, repo.id, "contents", true)
        .await
        .map_err(crate::auth::scope_error)?;
    let authority = auth.database_actor();
    let actor = auth.user;
    if !codeyard_identity::repository_access::can_write(&state.pool, &actor, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::new(StatusCode::FORBIDDEN, "Repository write denied"));
    }
    let input: Merge = if body.is_empty() {
        Merge::default()
    } else {
        serde_json::from_slice(&body).map_err(codeyard_types::github_error::json_parse_error)?
    };
    let method = MergeMethod::parse(input.merge_method.as_deref().unwrap_or("merge"))
        .ok_or_else(|| Error::invalid("Invalid merge method"))?;
    let (_, mut pull) = target(&state, &headers, &owner, &name, number).await?;
    if pull.state == "open" {
        codeyard_ci::pull_requests::branches::refresh_open_pull_request(
            &state.pool,
            &repo,
            &state.ci_runs,
            actor.id,
            pull.id,
            Some(codeyard_database::models::AccessLevel::Owner),
        )
        .await
        .map_err(Error::unavailable)?;
        pull =
            codeyard_db_repos::pull_requests::records::get_pull_request_by_id(&state.pool, pull.id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
    }
    if input
        .sha
        .as_deref()
        .is_some_and(|sha| !sha.eq_ignore_ascii_case(&pull.head_sha))
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "Head branch was modified. Review and try the merge again.",
        ));
    }
    if pull.merged {
        let sha = pull
            .merge_commit_sha
            .ok_or_else(|| Error::unavailable(()))?;
        return Ok(Json(
            json!({"sha": sha, "merged": true, "message": "Pull request is already merged"}),
        ));
    }
    let result = codeyard_repos_pulls::repositories::pull_requests::merge::merge_pull_request(
        &state,
        authority,
        (owner, name, number),
        codeyard_repos_pulls::repositories::pull_requests::merge::MergeRequest {
            expected_head: pull.head_sha,
            expected_base: pull.base_sha,
            expected_base_branch: Some(pull.base_branch),
            expected_target: None,
            merge_method: method,
            commit_title: input.commit_title,
            commit_message: input.commit_message,
            author_email: None,
        },
    )
    .await
    .map_err(operation_error)?;
    Ok(Json(
        json!({"sha": result.sha, "merged": result.merged, "message": result.message}),
    ))
}

#[derive(Default, Deserialize)]
struct UpdateBranch {
    expected_head_sha: Option<String>,
}

async fn update_branch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>)> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = viewer.ok_or_else(codeyard_identity::github_auth::required)?;
    // The update moves the head branch, which may live in a fork, so the credential needs
    // contents write on the head repository.
    let pull =
        codeyard_db_repos::pull_requests::records::get_pull_request(&state.pool, repo.id, number)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    auth.repository_scope(
        &state.pool,
        pull.head_repo_id
            .ok_or_else(|| Error::invalid("Head branch is unavailable"))?,
        "contents",
        true,
    )
    .await
    .map_err(crate::auth::scope_error)?;
    let token = auth.token.id;
    let actor = auth.user;
    let input: Option<UpdateBranch> = if body.is_empty() {
        None
    } else {
        serde_json::from_slice(&body).map_err(codeyard_types::github_error::json_parse_error)?
    };
    let input = input.unwrap_or_default();
    codeyard_repos_pulls::repositories::pull_requests::update_branch::update_branch(
        &state.pool,
        &state.ci_runs,
        actor.id,
        (&owner, &name, number),
        input.expected_head_sha.as_deref(),
        false,
        Some(token),
    )
    .await
    .map_err(crate::pulls::mutations::operation_error)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(
            json!({"message":"Pull request branch is up to date.","url":format!("{}/pulls/{number}",codeyard_types::urls::repo_url(&owner,&name))}),
        ),
    ))
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("pulls/merge", put(merge_pull)),
        ("pulls/update-branch", put(update_branch)),
    ]
}

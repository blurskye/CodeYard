use crate::pulls::target;
use axum::body::Bytes;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_repos_core::store_types::MergeMethod;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

#[derive(Default, Deserialize)]
struct Input {
    sha: Option<String>,
    merge_method: Option<String>,
    merge_action: Option<String>,
    bypass_rules: Option<bool>,
    commit_title: Option<String>,
    commit_message: Option<String>,
}
fn failed(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"status":"failed","details":{"message":message.into()}})),
    )
        .into_response()
}
async fn start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, String)>,
    body: Bytes,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    // The merge moves the base branch, so the credential needs contents write as well as the
    // pull request grant the route asked for.
    auth.repository_scope(&state.pool, repo.id, "contents", true)
        .await
        .map_err(crate::auth::scope_error)?;
    if !codeyard_identity::repository_access::can_write(&state.pool, &auth.user, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::new(StatusCode::FORBIDDEN, "Repository write denied"));
    }
    let Ok(number) = number.parse::<i32>() else {
        return Ok(failed(
            StatusCode::BAD_REQUEST,
            "Invalid pull request number",
        ));
    };
    let input: Option<Input> = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice(&body) {
            Ok(input) => input,
            Err(error) => {
                let error = codeyard_model::github_error::json_parse_error(error);
                return Ok(failed(error.0, error.1));
            }
        }
    };
    let input = input.unwrap_or_default();
    let method = MergeMethod::parse(input.merge_method.as_deref().unwrap_or("merge"))
        .ok_or_else(|| Error::validation("PullRequest", "merge_method", "invalid"))?;
    let action = input.merge_action.as_deref().unwrap_or("default");
    if !matches!(action, "default" | "direct_merge") {
        return Err(Error::validation(
            "PullRequest",
            "merge_action",
            "unsupported",
        ));
    }
    if input.bypass_rules == Some(true) {
        return Err(Error::validation(
            "PullRequest",
            "bypass_rules",
            "unsupported",
        ));
    }
    let (_, mut pull) = target(&state, &headers, &owner, &name, number).await?;
    if pull.state == "open" {
        codeyard_repos_core::repositories::pull_requests::branches::refresh_open_pull_request(
            &state.pool,
            &repo,
            &state.ci_runs,
            auth.user.id,
            pull.id,
            Some(codeyard_db_core::models::AccessLevel::Owner),
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
        return Ok(failed(
            StatusCode::CONFLICT,
            "Head branch was modified. Review and try the merge again.",
        ));
    }
    if pull.merged {
        let sha = pull
            .merge_commit_sha
            .ok_or_else(|| Error::unavailable(()))?;
        return Ok(Json(json!({"status":"merged","details":{"message":"Pull request is already merged","sha":sha}})).into_response());
    }
    if pull.state != "open" || pull.is_draft {
        return Ok(failed(
            StatusCode::BAD_REQUEST,
            "Only an open, non-draft pull request can be merged",
        ));
    }
    let request = codeyard_repos_pulls::repositories::pull_requests::merge::MergeRequest {
        expected_head: pull.head_sha,
        expected_base: pull.base_sha,
        expected_base_branch: Some(pull.base_branch),
        expected_target: None,
        merge_method: method,
        commit_title: input.commit_title,
        commit_message: input.commit_message,
        author_email: None,
    };
    match codeyard_repos_pulls::repositories::pull_requests::async_merge::start(
        &state,
        codeyard_repos_pulls::repositories::pull_requests::async_merge::Job {
            repo_id: repo.id,
            pull_request_id: pull.id,
            authority: auth.database_actor(),
            owner,
            name,
            number,
            merge: request,
            action: action.into(),
        },
    )
    .await
    {
        Ok(value) => Ok((StatusCode::ACCEPTED, Json(value)).into_response()),
        Err(codeyard_repos_core::repositories::pull_requests::OperationError::Conflict {
            message,
            ..
        }) => Ok(failed(StatusCode::CONFLICT, message)),
        Err(error) => Err(crate::pulls::mutations::operation_error(error)),
    }
}
async fn result(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number, uuid)): Path<(String, String, String, String)>,
) -> Result<Json<Value>> {
    let number = number.parse::<i32>().map_err(|_| Error::missing())?;
    let uuid = uuid::Uuid::parse_str(&uuid)
        .map_err(|_| Error::missing())?
        .to_string();
    let (repo, pull) = target(&state, &headers, &owner, &name, number).await?;
    let request =
        codeyard_db_repos::pull_requests::async_merge::get(&state.pool, repo.id, pull.id, &uuid)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    Ok(Json(
        json!({"status":request.status,"details":request.details}),
    ))
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("pulls/merge-async", put(start)),
        ("pulls/get-merge-async-result", get(result)),
    ]
}

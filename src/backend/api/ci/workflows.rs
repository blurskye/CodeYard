use crate::backend::api::ci::responses::WorkflowInfo;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::app::AppState;
use crate::backend::ci::admission::is_safe_owner;
use crate::backend::ci::list_workflows_for_ref;

use crate::backend::repositories::store::repo_store;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct ListWorkflowsQuery {
    pub owner: String,
    pub repo: String,
    pub git_ref: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct ListWorkflowsResponse {
    pub workflows: Vec<WorkflowInfo>,
}

pub async fn list_ci_workflows(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ListWorkflowsQuery>,
) -> Result<Json<ListWorkflowsResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;

    if !is_safe_owner(&query.owner)
        || crate::backend::repositories::lifecycle::validate_name(&query.repo).is_err()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "owner and repo must use safe repository characters",
            )),
        ));
    }

    let repository =
        crate::backend::db::repositories::get_repository(&state.pool, &query.owner, &query.repo)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError::new(
                        "database_error",
                        "Repository data is unavailable",
                    )),
                )
            })?
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ApiError::new("not_found", "Repository not found")),
                )
            })?;

    crate::backend::api::ci::jobs::ensure_repo_read_access(&state, &user, &repository).await?;

    let git_ref = query
        .git_ref
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&repository.default_branch);

    let store = repo_store(&repository, state.cluster.as_ref());
    let workflows = match list_workflows_for_ref(&store, git_ref).await {
        Ok(workflows) => workflows,
        Err(error)
            if error
                .downcast_ref::<codeyard_git::error::RepoError>()
                .is_some_and(|error| error.kind == codeyard_git::error::ErrorKind::NotFound) =>
        {
            if query.git_ref.is_none() {
                Vec::new()
            } else {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(ApiError::new("not_found", "Workflow ref not found")),
                ));
            }
        }
        Err(_) => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new(
                    "ci_error",
                    "Workflow definitions are unavailable",
                )),
            ))
        }
    };
    let user = get_current_user(&state.pool, &headers).await?;
    crate::backend::api::ci::jobs::ensure_repo_read_access(&state, &user, &repository).await?;

    Ok(Json(ListWorkflowsResponse {
        workflows: workflows
            .into_iter()
            .map(crate::backend::api::ci::responses::map_workflow_info)
            .collect(),
    }))
}

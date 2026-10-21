use super::responses::WorkflowInfo;
use crate::backend::{
    api::{http::error::ApiError, http::session::get_current_user},
    app::AppState,
    ci::{admission::is_safe_owner, list_workflows_for_ref},
    db,
    repositories::store::repo_store,
};
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

    let repository = db::repositories::get_repository(&state.pool, &query.owner, &query.repo)
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

    super::jobs::ensure_repo_read_access(&state, &user, &repository).await?;

    let git_ref = query
        .git_ref
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&repository.default_branch);

    let store = repo_store(&repository, state.cluster.as_ref());
    let workflows = async {
        // A repository with no commits has no workflows yet.
        if store.refs().await?.is_empty() {
            return Ok(Vec::new());
        }
        list_workflows_for_ref(&store, git_ref).await
    }
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new(
                "ci_error",
                "Workflow definitions are unavailable",
            )),
        )
    })?;

    Ok(Json(ListWorkflowsResponse {
        workflows: workflows
            .into_iter()
            .map(super::responses::map_workflow_info)
            .collect(),
    }))
}

use super::super::responses::{map_job_info, JobInfo};
use super::{ensure_repo_read_access, missing, CiReadError};
use crate::backend::{
    api::{http::error::ApiError, http::session::get_current_user},
    app::AppState,
    db,
};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct ListJobsQuery {
    pub owner: Option<String>,
    pub repo: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListJobsResponse {
    pub jobs: Vec<JobInfo>,
}

pub async fn list_ci_jobs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ListJobsQuery>,
) -> Result<Json<ListJobsResponse>, CiReadError> {
    let user = get_current_user(&state.pool, &headers).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    if query.owner.is_some() != query.repo.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "owner and repo must be supplied together",
            )),
        ));
    }
    let runs = if let (Some(owner), Some(repo)) = (query.owner.as_ref(), query.repo.as_ref()) {
        let repo = db::repositories::records::get_repository(&state.pool, owner, repo)
            .await
            .map_err(super::unavailable)?
            .ok_or_else(missing)?;
        ensure_repo_read_access(&state, &user, &repo).await?;
        db::ci::list_ci_runs_for_repo(&state.pool, repo.id, limit)
            .await
            .map_err(super::unavailable)?
    } else {
        // Current repository visibility is in SQL, BEFORE LIMIT; historical
        // triggering is not continuing private-resource authority.
        db::ci::list_ci_runs_for_user(&state.pool, user.id, limit)
            .await
            .map_err(super::unavailable)?
    };
    Ok(Json(ListJobsResponse {
        jobs: runs.into_iter().map(map_job_info).collect(),
    }))
}

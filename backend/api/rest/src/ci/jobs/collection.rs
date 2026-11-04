use crate::ci::responses::map_job_info;
use crate::ci::responses::JobInfo;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::session::get_current_user;
use codeyard_api_http::shared::ci::jobs::ensure_repo_read_access;
use codeyard_api_http::shared::ci::jobs::missing;
use codeyard_api_http::shared::ci::jobs::CiReadError;
use codeyard_app::app::AppState;
use serde::Deserialize;
use serde::Serialize;
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
        let repo =
            codeyard_db_repos::repositories::records::get_repository(&state.pool, owner, repo)
                .await
                .map_err(codeyard_api_http::shared::ci::jobs::unavailable)?
                .ok_or_else(missing)?;
        ensure_repo_read_access(&state, &user, &repo).await?;
        codeyard_db_actions::ci::list_filtered_ci_runs(
            &state.pool,
            user.id,
            Some(repo.id),
            &codeyard_db_actions::ci::CiRunFilter::default(),
            limit,
        )
        .await
        .map_err(codeyard_api_http::shared::ci::jobs::unavailable)?
    } else {
        // Current repository visibility is in SQL, BEFORE LIMIT; historical
        // triggering is not continuing private-resource authority.
        codeyard_db_actions::ci::list_ci_runs_for_user(&state.pool, user.id, limit)
            .await
            .map_err(codeyard_api_http::shared::ci::jobs::unavailable)?
    };
    Ok(Json(ListJobsResponse {
        jobs: runs.into_iter().map(map_job_info).collect(),
    }))
}

use crate::backend::api::error::ApiError;
use crate::backend::api::repos::access::{access_error, check_repo_read_access};
use crate::backend::app::AppState;
use crate::backend::db;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
#[derive(Serialize)]
pub struct WatchResponse {
    pub watched: bool,
    pub watchers_count: i32,
}

pub async fn watch_repo(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<WatchResponse>, (StatusCode, Json<ApiError>)> {
    let actor = crate::backend::api::session::get_current_user(&state.pool, &headers).await?;
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(|_| access_error(crate::backend::repositories::access::AccessError::Unavailable))?
        .ok_or_else(|| access_error(crate::backend::repositories::access::AccessError::NotFound))?;
    let count = crate::backend::repositories::engagement::change(
        &state.pool,
        &state.notification_broadcaster,
        actor.id,
        repository.id,
        crate::backend::repositories::engagement::Engagement::Watch,
        true,
    )
    .await
    .map_err(super::responses::engagement_error)?;
    Ok(Json(WatchResponse {
        watched: true,
        watchers_count: count,
    }))
}

pub async fn unwatch_repo(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<WatchResponse>, (StatusCode, Json<ApiError>)> {
    let actor = crate::backend::api::session::get_current_user(&state.pool, &headers).await?;
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(|_| access_error(crate::backend::repositories::access::AccessError::Unavailable))?
        .ok_or_else(|| access_error(crate::backend::repositories::access::AccessError::NotFound))?;
    let count = crate::backend::repositories::engagement::change(
        &state.pool,
        &state.notification_broadcaster,
        actor.id,
        repository.id,
        crate::backend::repositories::engagement::Engagement::Watch,
        false,
    )
    .await
    .map_err(super::responses::engagement_error)?;
    Ok(Json(WatchResponse {
        watched: false,
        watchers_count: count,
    }))
}

pub async fn check_watch(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<WatchResponse>, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    "database_error",
                    "Repository operation is unavailable",
                )),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    check_repo_read_access(&state.pool, &repository, &headers).await?;
    let watched =
        match crate::backend::api::session::get_optional_user(&state.pool, &headers).await? {
            Some(user) => {
                db::repositories::engagement::is_repo_watched(&state.pool, user.id, repository.id)
                    .await
                    .map_err(|_| {
                        access_error(crate::backend::repositories::access::AccessError::Unavailable)
                    })?
            }
            None => false,
        };

    Ok(Json(WatchResponse {
        watched,
        watchers_count: repository.watchers_count,
    }))
}

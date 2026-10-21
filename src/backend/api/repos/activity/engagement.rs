use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::{get_current_user, get_optional_user};
use crate::backend::api::repos::access::{access_error, find_repository, readable_repository};
use crate::backend::api::repos::chrome::ApiResult;
use crate::backend::api::repos::responses::engagement_error;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::access::AccessError;
use crate::backend::repositories::engagement::{change, Engagement};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
#[derive(Serialize)]
pub struct StarResponse {
    pub starred: bool,
    pub stars_count: i32,
}

#[derive(Serialize)]
pub struct WatchResponse {
    pub watched: bool,
    pub watchers_count: i32,
}

/// Stars or watches the repository for the signed-in user, or removes it. Returns the new count.
async fn set_engagement(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    repo: &str,
    kind: Engagement,
    on: bool,
) -> ApiResult<i32> {
    let actor = get_current_user(&state.pool, headers).await?;
    let repository = find_repository(&state.pool, owner, repo).await?;
    change(
        &state.pool,
        &state.notification_broadcaster,
        actor.id,
        repository.id,
        kind,
        on,
    )
    .await
    .map_err(engagement_error)
}

/// Whether the signed-in user has starred or watches the repository, with the repository.
async fn viewer_engagement(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    repo: &str,
    kind: Engagement,
) -> ApiResult<(bool, db::models::Repository)> {
    let repository = readable_repository(&state.pool, owner, repo, headers).await?;
    let Some(user) = get_optional_user(&state.pool, headers).await? else {
        return Ok((false, repository));
    };
    let engaged = match kind {
        Engagement::Star => {
            db::repositories::engagement::is_repo_starred(&state.pool, user.id, repository.id).await
        }
        Engagement::Watch => {
            db::repositories::engagement::is_repo_watched(&state.pool, user.id, repository.id).await
        }
    }
    .map_err(|_| access_error(AccessError::Unavailable))?;
    Ok((engaged, repository))
}

type JsonResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

pub async fn star_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> JsonResult<StarResponse> {
    let stars_count =
        set_engagement(&state, &headers, &owner, &repo, Engagement::Star, true).await?;
    Ok(Json(StarResponse {
        starred: true,
        stars_count,
    }))
}

pub async fn unstar_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> JsonResult<StarResponse> {
    let stars_count =
        set_engagement(&state, &headers, &owner, &repo, Engagement::Star, false).await?;
    Ok(Json(StarResponse {
        starred: false,
        stars_count,
    }))
}

pub async fn check_star(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> JsonResult<StarResponse> {
    let (starred, repository) =
        viewer_engagement(&state, &headers, &owner, &repo, Engagement::Star).await?;
    Ok(Json(StarResponse {
        starred,
        stars_count: repository.stars_count,
    }))
}

pub async fn watch_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> JsonResult<WatchResponse> {
    let watchers_count =
        set_engagement(&state, &headers, &owner, &repo, Engagement::Watch, true).await?;
    Ok(Json(WatchResponse {
        watched: true,
        watchers_count,
    }))
}

pub async fn unwatch_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> JsonResult<WatchResponse> {
    let watchers_count =
        set_engagement(&state, &headers, &owner, &repo, Engagement::Watch, false).await?;
    Ok(Json(WatchResponse {
        watched: false,
        watchers_count,
    }))
}

pub async fn check_watch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> JsonResult<WatchResponse> {
    let (watched, repository) =
        viewer_engagement(&state, &headers, &owner, &repo, Engagement::Watch).await?;
    Ok(Json(WatchResponse {
        watched,
        watchers_count: repository.watchers_count,
    }))
}

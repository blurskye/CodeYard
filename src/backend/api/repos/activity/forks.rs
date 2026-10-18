use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{json_page, PageQuery};
use crate::backend::api::repos::responses::{
    repository_owners, repository_topics, ForkParent, RepoResponse,
};
use crate::backend::app::AppState;
use crate::backend::db;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
#[derive(Serialize)]
pub struct ForkResponse {
    pub repo: RepoResponse,
    pub message: String,
}

#[derive(Serialize)]
pub struct CheckForkResponse {
    pub has_fork: bool,
    pub fork: Option<ForkInfo>,
}

#[derive(Serialize)]
pub struct ForkInfo {
    pub owner: String,
    pub name: String,
    pub full_name: String,
}

pub async fn fork_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<(StatusCode, Json<ForkResponse>), (StatusCode, Json<ApiError>)> {
    let pool = state.pool.clone();
    let user = get_current_user(&pool, &headers).await?;
    let source_repo = db::repositories::records::get_repository(&pool, &owner, &repo)
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
    check_repo_read_access(&pool, &source_repo, &headers).await?;
    let forked_repo = crate::backend::repositories::lifecycle::fork(
        &pool,
        state.cluster.as_ref(),
        &state.repos_path,
        &state.notification_broadcaster,
        user.id,
        source_repo.id,
        &owner,
    )
    .await
    .map_err(crate::backend::api::repos::responses::lifecycle_error)?;

    let forked_from_info = ForkParent {
        id: source_repo.id,
        owner: owner.clone(),
        name: source_repo.name.clone(),
        full_name: format!("{}/{}", owner, source_repo.name),
    };

    Ok((
        StatusCode::CREATED,
        Json(ForkResponse {
            repo: RepoResponse {
                id: forked_repo.id,
                owner_id: forked_repo.owner_id(),
                owner: user.username.clone(),
                name: forked_repo.name.clone(),
                full_name: format!("{}/{}", user.username, forked_repo.name),
                description: forked_repo.description,
                website: forked_repo.website,
                // A fork starts without the source's topics.
                topics: Vec::new(),
                is_private: forked_repo.is_private,
                is_archived: forked_repo.is_archived,
                allow_merge_commit: forked_repo.allow_merge_commit,
                allow_squash_merge: forked_repo.allow_squash_merge,
                allow_rebase_merge: forked_repo.allow_rebase_merge,
                default_branch: forked_repo.default_branch,
                stars_count: forked_repo.stars_count,
                forks_count: forked_repo.forks_count,
                watchers_count: forked_repo.watchers_count,
                forked_from: Some(forked_from_info),
                created_at: forked_repo.created_at.to_rfc3339(),
                updated_at: forked_repo.updated_at.to_rfc3339(),
            },
            message: format!(
                "Successfully forked {}/{} to {}/{}",
                owner, source_repo.name, user.username, forked_repo.name
            ),
        }),
    ))
}

pub async fn check_fork(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<CheckForkResponse>, (StatusCode, Json<ApiError>)> {
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
    if let Some(user) =
        crate::backend::api::http::session::get_optional_user(&state.pool, &headers).await?
    {
        if let Some(fork) =
            db::repositories::discovery::get_user_fork(&state.pool, user.id, repository.id)
                .await
                .map_err(|_| {
                    crate::backend::api::repos::access::access_error(
                        crate::backend::repositories::access::AccessError::Unavailable,
                    )
                })?
        {
            return Ok(Json(CheckForkResponse {
                has_fork: true,
                fork: Some(ForkInfo {
                    owner: user.username.clone(),
                    name: fork.name.clone(),
                    full_name: format!("{}/{}", user.username, fork.name),
                }),
            }));
        }
    }
    Ok(Json(CheckForkResponse {
        has_fork: false,
        fork: None,
    }))
}

pub async fn list_forks(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<PageQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<RepoResponse>>, (StatusCode, Json<ApiError>)> {
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
    let (offset, limit) = json_page(query.page, query.per_page)?;
    let forks = db::repositories::discovery::list_forks(
        &state.pool,
        repository.id,
        limit as i64,
        offset as i64,
    )
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new(
                "database_error",
                "Repository operation is unavailable",
            )),
        )
    })?;
    let user = if forks.iter().any(|fork| fork.is_private) {
        crate::backend::api::http::session::get_optional_user(&state.pool, &headers).await?
    } else {
        None
    };
    let mut visible_forks = Vec::new();
    // Authorize only this bounded candidate page; public rows need no reload.
    for fork in forks {
        if crate::backend::repositories::access::can_read(&state.pool, user.as_ref(), &fork)
            .await
            .map_err(|_| {
                crate::backend::api::repos::access::access_error(
                    crate::backend::repositories::access::AccessError::Unavailable,
                )
            })?
        {
            visible_forks.push(fork);
        }
    }
    let ids = std::iter::once(repository.id)
        .chain(visible_forks.iter().map(|fork| fork.id))
        .collect::<Vec<_>>();
    let mut owners = repository_owners(&state.pool, &ids).await?;
    let mut topics = repository_topics(&state.pool, &ids).await?;
    let owner = owners.remove(&repository.id).expect("validated owner");
    let parent_info = ForkParent {
        id: repository.id,
        owner: owner.clone(),
        name: repo.clone(),
        full_name: format!("{}/{}", owner, repo),
    };

    let mut responses = Vec::with_capacity(visible_forks.len());
    for fork in visible_forks {
        let fork_owner = owners.remove(&fork.id).expect("validated owner");
        responses.push(RepoResponse {
            id: fork.id,
            owner_id: fork.owner_id(),
            owner: fork_owner.clone(),
            name: fork.name.clone(),
            full_name: format!("{}/{}", fork_owner, fork.name),
            description: fork.description,
            website: fork.website,
            topics: topics.remove(&fork.id).unwrap_or_default(),
            is_private: fork.is_private,
            is_archived: fork.is_archived,
            allow_merge_commit: fork.allow_merge_commit,
            allow_squash_merge: fork.allow_squash_merge,
            allow_rebase_merge: fork.allow_rebase_merge,
            default_branch: fork.default_branch,
            stars_count: fork.stars_count,
            forks_count: fork.forks_count,
            watchers_count: fork.watchers_count,
            forked_from: Some(parent_info.clone()),
            created_at: fork.created_at.to_rfc3339(),
            updated_at: fork.updated_at.to_rfc3339(),
        });
    }
    Ok(Json(responses))
}

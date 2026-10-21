use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::readable_repository;
use crate::backend::api::repos::paging::{json_page, PageQuery};
use crate::backend::api::repos::responses::{
    repository_owners, repository_topics, repository_unavailable, ForkParent, RepoResponse,
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
    let source_repo = readable_repository(&pool, &owner, &repo, &headers).await?;
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
            // A fork starts without the source's topics.
            repo: RepoResponse::new(
                &forked_repo,
                &user.username,
                Vec::new(),
                Some(forked_from_info),
            ),
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
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
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
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    let (offset, limit) = json_page(query.page, query.per_page)?;
    let forks = db::repositories::discovery::list_forks(
        &state.pool,
        repository.id,
        limit as i64,
        offset as i64,
    )
    .await
    .map_err(|_| repository_unavailable())?;
    let user = if forks.iter().any(|fork| fork.is_private) {
        crate::backend::api::http::session::get_optional_user(&state.pool, &headers).await?
    } else {
        None
    };
    // Preserve candidate-page pagination, but authorize the private candidates together.
    let private_ids: Vec<_> = forks
        .iter()
        .filter(|fork| fork.is_private)
        .map(|fork| fork.id)
        .collect();
    let readable = if private_ids.is_empty() || user.is_none() {
        Vec::new()
    } else {
        db::repositories::access::readable_repository_ids(
            &state.pool,
            user.as_ref().map(|user| user.id),
            &private_ids,
        )
        .await
        .map_err(|_| {
            super::super::access::access_error(
                crate::backend::repositories::access::AccessError::Unavailable,
            )
        })?
    };
    let visible_forks: Vec<_> = forks
        .into_iter()
        .filter(|fork| !fork.is_private || readable.contains(&fork.id))
        .collect();
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
        let topics = topics.remove(&fork.id).unwrap_or_default();
        responses.push(RepoResponse::new(
            &fork,
            &fork_owner,
            topics,
            Some(parent_info.clone()),
        ));
    }
    Ok(Json(responses))
}

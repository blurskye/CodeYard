use crate::access::readable_repository;
use crate::paging::json_page;
use crate::paging::PageQuery;
use crate::responses::repository_owners;
use crate::responses::repository_topics;
use crate::responses::ForkParent;
use crate::responses::RepoResponse;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::session::get_current_user;
use codeyard_api_http::shared::repos::responses::repository_fork_counts;
use codeyard_api_http::shared::repos::responses::repository_unavailable;
use codeyard_app::app::AppState;
use codeyard_db_core::models::Repository;
use codeyard_db_core::DbPool;
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
    let forked_repo = codeyard_repos_core::repositories::lifecycle::fork(
        &pool,
        state.cluster.as_ref(),
        &state.repos_path,
        &state.notification_broadcaster,
        codeyard_db_core::queries::credentials::Actor::from((
            user.id,
            codeyard_api_http::session::extract_session_id(&headers).unwrap_or_default(),
        )),
        source_repo.id,
        &owner,
    )
    .await
    .map_err(crate::responses::lifecycle_error)?;

    let forked_from_info = ForkParent {
        id: source_repo.id,
        owner: owner.clone(),
        name: source_repo.name.clone(),
        full_name: format!("{}/{}", owner, source_repo.name),
    };

    let topics = repository_topics(&pool, &[forked_repo.id])
        .await?
        .remove(&forked_repo.id)
        .unwrap_or_default();
    let forks = repository_fork_counts(&pool, Some(&headers), &[forked_repo.id])
        .await?
        .remove(&forked_repo.id)
        .unwrap_or_default();
    Ok((
        StatusCode::CREATED,
        Json(ForkResponse {
            repo: RepoResponse::new(
                &forked_repo,
                &user.username,
                topics,
                Some(forked_from_info),
                forks,
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
    if let Some(user) = codeyard_api_http::session::get_optional_user(&state.pool, &headers).await?
    {
        if let Some(fork) = codeyard_db_repos::repositories::discovery::get_user_fork(
            &state.pool,
            user.id,
            repository.id,
        )
        .await
        .map_err(|_| {
            codeyard_api_http::shared::repos::access::access_error(
                codeyard_identity::repository_access::AccessError::Unavailable,
            )
        })? {
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

/// Visibility is applied before paging, so hidden forks cannot create empty pages or links.
pub(crate) async fn visible_forks(
    pool: &DbPool,
    viewer: Option<i64>,
    session: &str,
    repo_id: i64,
    limit: i64,
    offset: i64,
) -> anyhow::Result<(Vec<Repository>, bool)> {
    let mut forks = codeyard_db_repos::repositories::discovery::list_visible_forks(
        pool,
        viewer,
        Some(session),
        repo_id,
        limit + 1,
        offset,
    )
    .await?;
    let more = forks.len() as i64 > limit;
    forks.truncate(limit as usize);
    Ok((forks, more))
}

pub(crate) async fn list_forks(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<PageQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<RepoResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    let (offset, limit) = json_page(query.page, query.per_page)?;
    let user = codeyard_api_http::session::get_optional_user(&state.pool, &headers).await?;
    let session = codeyard_api_http::session::extract_session_id(&headers).unwrap_or_default();
    let (visible_forks, _) = visible_forks(
        &state.pool,
        user.map(|user| user.id),
        &session,
        repository.id,
        limit as i64,
        offset as i64,
    )
    .await
    .map_err(|_| repository_unavailable())?;
    let ids = std::iter::once(repository.id)
        .chain(visible_forks.iter().map(|fork| fork.id))
        .collect::<Vec<_>>();
    let mut owners = repository_owners(&state.pool, &ids).await?;
    let mut topics = repository_topics(&state.pool, &ids).await?;
    let mut fork_counts = repository_fork_counts(&state.pool, Some(&headers), &ids).await?;
    // Enrichment may have observed topics written after a privacy or membership change.
    let viewer = codeyard_api_http::session::get_optional_user(&state.pool, &headers).await?;
    let mut readable = std::collections::HashSet::new();
    for ids in ids.chunks(100) {
        readable.extend(
            codeyard_db_repos::repositories::access::readable_repository_ids(
                &state.pool,
                viewer.as_ref().map(|user| user.id),
                ids,
            )
            .await
            .map_err(|_| repository_unavailable())?,
        );
    }
    codeyard_api_http::shared::repos::access::check_repo_read_access(
        &state.pool,
        &repository,
        &headers,
    )
    .await?;
    let owner = owners.remove(&repository.id).expect("validated owner");
    let parent_info = ForkParent {
        id: repository.id,
        owner: owner.clone(),
        name: repo.clone(),
        full_name: format!("{}/{}", owner, repo),
    };

    let mut responses = Vec::with_capacity(visible_forks.len());
    for fork in visible_forks {
        if !readable.contains(&fork.id) {
            continue;
        }
        let fork_owner = owners.remove(&fork.id).expect("validated owner");
        let topics = topics.remove(&fork.id).unwrap_or_default();
        responses.push(RepoResponse::new(
            &fork,
            &fork_owner,
            topics,
            Some(parent_info.clone()),
            fork_counts.remove(&fork.id).unwrap_or_default(),
        ));
    }
    Ok(Json(responses))
}

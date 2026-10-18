use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_optional_user;
use crate::backend::api::repos::responses::{repository_topics, RepoResponse};
use crate::backend::app::AppState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use std::sync::Arc;

pub async fn list_user_repos(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<RepoResponse>>, (StatusCode, Json<ApiError>)> {
    let current_user = get_optional_user(&state.pool, &headers).await?;
    let (_, repos) = crate::backend::identity::profiles::repositories_on_profile(
        &state.pool,
        &username,
        current_user.map(|user| user.id),
    )
    .await
    .map_err(|error| match error {
        crate::backend::identity::profiles::ProfileReadError::Missing => (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "User not found")),
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new(
                "database_error",
                "Account storage is temporarily unavailable",
            )),
        ),
    })?;
    let ids = repos.iter().map(|repo| repo.id).collect::<Vec<_>>();
    let mut topics = repository_topics(&state.pool, &ids).await?;
    let mut responses = Vec::new();
    for repository in repos {
        responses.push(RepoResponse {
            id: repository.id,
            owner_id: repository.owner_id(),
            owner: username.clone(),
            name: repository.name.clone(),
            full_name: format!("{}/{}", username, repository.name),
            description: repository.description,
            website: repository.website,
            topics: topics.remove(&repository.id).unwrap_or_default(),
            is_private: repository.is_private,
            is_archived: repository.is_archived,
            allow_merge_commit: repository.allow_merge_commit,
            allow_squash_merge: repository.allow_squash_merge,
            allow_rebase_merge: repository.allow_rebase_merge,
            default_branch: repository.default_branch,
            stars_count: repository.stars_count,
            forks_count: repository.forks_count,
            watchers_count: repository.watchers_count,
            forked_from: None,
            created_at: repository.created_at.to_rfc3339(),
            updated_at: repository.updated_at.to_rfc3339(),
        });
    }

    Ok(Json(responses))
}

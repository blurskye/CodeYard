use crate::access::readable_repository;
use crate::responses::repo_store_error;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::shared::repos::access::access_error;
use codeyard_app::app::AppState;
use codeyard_git::error::RepoError;
use codeyard_git_storage::repositories::store::repo_store;
use codeyard_git_storage::store_types::MAX_CONTRIBUTOR_RESPONSE_BYTES;
use codeyard_identity::repository_access::AccessError;
use serde::Serialize;
use std::sync::Arc;

fn add_identity_bytes(total: &mut usize, name: &str, email: &str) -> Result<(), RepoError> {
    *total = total
        .checked_add(name.len())
        .and_then(|bytes| bytes.checked_add(email.len()))
        .filter(|bytes| *bytes <= MAX_CONTRIBUTOR_RESPONSE_BYTES)
        .ok_or_else(RepoError::response_too_large)?;
    Ok(())
}

#[derive(Serialize)]
pub struct ContributorResponse {
    pub name: String,
    pub email: String,
    pub commits: i32,
    pub user_id: Option<i64>,
    pub avatar_url: Option<String>,
}

pub async fn get_repo_contributors(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<ContributorResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;

    // Git's ref-aware cache follows pushes, force-pushes and branch deletions.
    let contributors = repo_store(&repository, state.cluster.as_ref())
        .contributors()
        .await
        .map_err(|error| repo_store_error(error, StatusCode::SERVICE_UNAVAILABLE))?;
    let mut response_bytes = 0usize;
    for contributor in &contributors {
        add_identity_bytes(&mut response_bytes, &contributor.name, &contributor.email)
            .map_err(|error| repo_store_error(error, StatusCode::SERVICE_UNAVAILABLE))?;
    }
    let users_by_email = contributor_accounts(&state.pool, &contributors)
        .await
        .map_err(|_| access_error(AccessError::Unavailable))?;
    let responses = contributors
        .into_iter()
        .map(|contributor| {
            let user = users_by_email.get(contributor.email.as_str());
            Ok(ContributorResponse {
                name: contributor.name,
                email: contributor.email,
                commits: i32::try_from(contributor.commits).map_err(|_| {
                    repo_store_error(
                        RepoError::response_too_large(),
                        StatusCode::SERVICE_UNAVAILABLE,
                    )
                })?,
                user_id: user.map(|user| user.id),
                avatar_url: user.and_then(|user| {
                    user.avatar_file_id
                        .map(|id| format!("/api/files/{id}/download"))
                }),
            })
        })
        .collect::<Result<Vec<_>, (StatusCode, Json<ApiError>)>>()?;
    Ok(Json(responses))
}

use codeyard_api_http::shared::repos::code::contributors::contributor_accounts;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contributor_identity_budget_accepts_the_limit_and_rejects_one_more() {
        let mut total = MAX_CONTRIBUTOR_RESPONSE_BYTES - 1;
        assert!(add_identity_bytes(&mut total, "x", "").is_ok());
        assert_eq!(total, MAX_CONTRIBUTOR_RESPONSE_BYTES);
        assert!(add_identity_bytes(&mut total, "x", "")
            .unwrap_err()
            .is_response_too_large());
    }
}

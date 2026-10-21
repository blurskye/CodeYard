use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::responses::repo_store_error;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::store::{repo_store, RepoError, MAX_CONTRIBUTOR_RESPONSE_BYTES};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Serialize;
use std::collections::HashMap;
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
    let emails: Vec<_> = contributors
        .iter()
        .map(|contributor| contributor.email.clone())
        .collect();
    let users = db::users::accounts::get_users_by_emails(&state.pool, &emails)
        .await
        .unwrap_or_default();
    // Commits made on the forge for an account without an email carry its no-reply address.
    let usernames: Vec<String> = emails
        .iter()
        .filter_map(|email| email.strip_suffix("@users.noreply"))
        .map(str::to_owned)
        .collect();
    let noreply_users = if usernames.is_empty() {
        Vec::new()
    } else {
        db::users::accounts::get_users_by_usernames(&state.pool, &usernames)
            .await
            .unwrap_or_default()
    };
    let mut users_by_email: HashMap<String, _> = users
        .iter()
        .filter_map(|user| user.email.clone().map(|email| (email, user)))
        .collect();
    users_by_email.extend(
        noreply_users
            .iter()
            .map(|user| (user.noreply_email(), user)),
    );
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

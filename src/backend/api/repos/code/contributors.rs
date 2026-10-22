use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::{access_error, readable_repository};
use crate::backend::api::repos::responses::repo_store_error;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::models::User;
use crate::backend::db::DbPool;
use crate::backend::repositories::access::AccessError;
use crate::backend::repositories::store::{
    repo_store, ContributorInfo, RepoError, MAX_CONTRIBUTOR_RESPONSE_BYTES,
};
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

/// The account behind each contributor's email, by email. Commits made on the forge for an
/// account without an email carry its no-reply address.
pub(crate) async fn contributor_accounts(
    pool: &DbPool,
    contributors: &[ContributorInfo],
) -> anyhow::Result<HashMap<String, User>> {
    let emails: Vec<_> = contributors
        .iter()
        .map(|contributor| contributor.email.clone())
        .collect();
    let users = db::users::accounts::get_users_by_emails(pool, &emails).await?;
    let usernames: Vec<String> = emails
        .iter()
        .filter_map(|email| email.strip_suffix("@users.noreply"))
        .map(str::to_owned)
        .collect();
    let noreply_users = if usernames.is_empty() {
        Vec::new()
    } else {
        db::users::accounts::get_users_by_usernames(pool, &usernames).await?
    };
    let mut users_by_email: HashMap<String, User> = users
        .into_iter()
        .filter_map(|user| user.email.clone().map(|email| (email, user)))
        .collect();
    users_by_email.extend(
        noreply_users
            .into_iter()
            .map(|user| (user.noreply_email(), user)),
    );
    Ok(users_by_email)
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

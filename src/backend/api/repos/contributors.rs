use crate::backend::api::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::store::{
    repo_store, RepoError, MAX_CONTRIBUTORS, MAX_CONTRIBUTOR_RESPONSE_BYTES,
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

    let db_contributors = db::repository_stats::get_repo_contributors(
        &state.pool,
        repository.id,
        (MAX_CONTRIBUTORS + 1) as i64,
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
    if db_contributors.len() > MAX_CONTRIBUTORS {
        return Err(crate::backend::api::repos::responses::repo_store_error(
            RepoError::response_too_large(),
            StatusCode::SERVICE_UNAVAILABLE,
        ));
    }

    let contributors: Vec<(String, String, i32, Option<i64>)> = if db_contributors.is_empty() {
        repo_store(&repository, state.cluster.as_ref())
            .contributors()
            .await
            .map_err(|error| {
                crate::backend::api::repos::responses::repo_store_error(
                    error,
                    StatusCode::SERVICE_UNAVAILABLE,
                )
            })?
            .into_iter()
            .map(|contributor| {
                i32::try_from(contributor.commits)
                    .map(|commits| (contributor.name, contributor.email, commits, None))
                    .map_err(|_| {
                        crate::backend::api::repos::responses::repo_store_error(
                            RepoError::response_too_large(),
                            StatusCode::SERVICE_UNAVAILABLE,
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        db_contributors
            .into_iter()
            .map(|contributor| {
                (
                    contributor.name,
                    contributor.email,
                    contributor.commits_count,
                    contributor.user_id,
                )
            })
            .collect()
    };

    let mut response_bytes = 0usize;
    for (name, email, _, _) in &contributors {
        add_identity_bytes(&mut response_bytes, name, email).map_err(|error| {
            crate::backend::api::repos::responses::repo_store_error(
                error,
                StatusCode::SERVICE_UNAVAILABLE,
            )
        })?;
    }

    let ids: Vec<_> = contributors
        .iter()
        .filter_map(|contributor| contributor.3)
        .collect();
    let emails: Vec<_> = contributors
        .iter()
        .filter(|contributor| contributor.3.is_none())
        .map(|contributor| contributor.1.clone())
        .collect();
    let (users_by_id, users_by_email) = tokio::join!(
        db::users::accounts::get_users_by_ids(&state.pool, &ids),
        db::users::accounts::get_users_by_emails(&state.pool, &emails),
    );
    let id_users = users_by_id.unwrap_or_default();
    let email_users = users_by_email.unwrap_or_default();
    let users_by_id: HashMap<_, _> = id_users.iter().map(|user| (user.id, user)).collect();
    let users_by_email: HashMap<_, _> = email_users
        .iter()
        .filter_map(|user| user.email.as_deref().map(|email| (email, user)))
        .collect();

    let mut responses = Vec::with_capacity(contributors.len());
    for (name, email, commits, stored_user_id) in contributors {
        let user = match stored_user_id {
            Some(id) => users_by_id.get(&id).copied(),
            None => users_by_email.get(email.as_str()).copied(),
        };
        responses.push(ContributorResponse {
            name,
            email,
            commits,
            user_id: stored_user_id.or_else(|| user.map(|user| user.id)),
            avatar_url: user.and_then(|user| {
                user.avatar_file_id
                    .map(|id| format!("/api/files/{id}/download"))
            }),
        });
    }

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

use crate::backend::api::error::ApiError;
use crate::backend::api::repos::commits::statuses::CommitStatusResponse;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::db;
use crate::backend::repositories::store::CommitInfo;
use axum::http::StatusCode;
use axum::Json;
use chrono::TimeZone;
use chrono::Utc;
use serde::Serialize;
/// Safe canonical namespace projection, never numeric-ID owner inference.
pub(crate) async fn repository_owner(
    pool: &db::DbPool,
    repository: &db::models::Repository,
) -> Result<String, (StatusCode, Json<ApiError>)> {
    let mut owners = repository_owners(pool, &[repository.id]).await?;
    Ok(owners.remove(&repository.id).expect("validated owner"))
}

pub(crate) async fn repository_owners(
    pool: &db::DbPool,
    ids: &[i64],
) -> Result<std::collections::BTreeMap<i64, String>, (StatusCode, Json<ApiError>)> {
    let unavailable = || {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::new(
                "unavailable",
                "Repository owner is unavailable",
            )),
        )
    };
    let owners = db::repositories::records::repository_owner_names(pool, ids)
        .await
        .map_err(|_| unavailable())?;
    if ids.iter().any(|id| !owners.contains_key(id)) {
        return Err(unavailable());
    }
    Ok(owners)
}

pub(crate) fn map_commit_status_response(
    status: db::models::CommitStatus,
    creator: Option<UserResponse>,
) -> CommitStatusResponse {
    CommitStatusResponse {
        id: status.id,
        sha: status.sha,
        context: status.context,
        state: status.state,
        description: status.description,
        target_url: status.target_url,
        creator_id: status.creator_id,
        creator,
        created_at: status.created_at.to_rfc3339(),
        updated_at: status.updated_at.to_rfc3339(),
    }
}

#[derive(Serialize)]
pub struct RepoResponse {
    pub id: i64,
    pub owner_id: i64,
    pub owner: String,
    pub name: String,
    pub full_name: String,
    pub description: Option<String>,
    pub is_private: bool,
    pub is_archived: bool,
    pub allow_merge_commit: bool,
    pub allow_squash_merge: bool,
    pub allow_rebase_merge: bool,
    pub default_branch: String,
    pub stars_count: i32,
    pub forks_count: i32,
    pub watchers_count: i32,
    pub forked_from: Option<ForkParent>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Serialize, Clone)]
pub struct ForkParent {
    pub id: i64,
    pub owner: String,
    pub name: String,
    pub full_name: String,
}

#[derive(Serialize, Clone)]
pub struct AuthorInfo {
    pub id: Option<i64>,
    pub username: Option<String>,
    pub avatar_url: Option<String>,
}

#[derive(Serialize)]
pub struct CommitResponse {
    pub sha: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub author_date: String,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_date: String,
    pub parents: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<AuthorInfo>,
}

impl CommitResponse {
    pub(crate) fn from_commit(commit: CommitInfo, author: Option<AuthorInfo>) -> Self {
        let date = |timestamp| {
            Utc.timestamp_opt(timestamp, 0)
                .single()
                .map(|date| date.to_rfc3339())
                .unwrap_or_default()
        };
        Self {
            sha: commit.sha,
            message: commit.message,
            author_name: commit.author_name,
            author_email: commit.author_email,
            author_date: date(commit.timestamp),
            committer_name: commit.committer_name,
            committer_email: commit.committer_email,
            committer_date: date(commit.committer_timestamp),
            parents: commit.parents,
            author,
        }
    }
}

pub(crate) fn repo_store_error(
    error: crate::backend::repositories::store::RepoError,
    fallback: StatusCode,
) -> (StatusCode, Json<ApiError>) {
    let (status, code, message) = if error.is_response_too_large() {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            "response_too_large",
            "Repository response exceeds its limit",
        )
    } else if error.is_storage_node_unavailable() {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "git_error",
            "storage node unavailable",
        )
    } else {
        use crate::backend::repositories::store::ErrorKind;
        let missing = matches!(error.kind, ErrorKind::NotFound | ErrorKind::MissingPath);
        (
            if missing {
                StatusCode::NOT_FOUND
            } else if error.kind == ErrorKind::Unavailable {
                // Unreachable storage is a service failure, whatever the caller falls back to.
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                fallback
            },
            "git_error",
            match error.kind {
                crate::backend::repositories::store::ErrorKind::NotFound
                | crate::backend::repositories::store::ErrorKind::MissingPath => {
                    "Requested Git resource not found"
                }
                crate::backend::repositories::store::ErrorKind::Invalid => "Invalid Git operation",
                crate::backend::repositories::store::ErrorKind::Conflict => "Git reference changed",
                crate::backend::repositories::store::ErrorKind::Unavailable => {
                    "Repository storage is unavailable"
                }
            },
        )
    };
    (status, Json(ApiError::new(code, message)))
}

pub(crate) fn lifecycle_status(
    error: &crate::backend::repositories::lifecycle::StorageError,
) -> StatusCode {
    use crate::backend::repositories::lifecycle::LifecycleFailure;
    match error.kind {
        LifecycleFailure::Invalid | LifecycleFailure::OwnRepositoryFork => StatusCode::BAD_REQUEST,
        LifecycleFailure::Denied => StatusCode::FORBIDDEN,
        LifecycleFailure::UnsupportedOrganizationFork => StatusCode::UNPROCESSABLE_ENTITY,
        LifecycleFailure::Conflict => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}

pub(crate) fn lifecycle_error(
    error: crate::backend::repositories::lifecycle::StorageError,
) -> (StatusCode, Json<ApiError>) {
    use crate::backend::repositories::lifecycle::LifecycleFailure;
    let code = match error.kind {
        LifecycleFailure::Invalid => "validation_error",
        LifecycleFailure::OwnRepositoryFork => "invalid_operation",
        LifecycleFailure::DeletedStorageRemains => "storage_unavailable",
        _ => "storage_pending",
    };
    (
        lifecycle_status(&error),
        Json(ApiError::new(code, error.message)),
    )
}

pub(crate) fn ref_error(
    error: crate::backend::repositories::refs::RefError,
) -> (StatusCode, Json<ApiError>) {
    use crate::backend::repositories::refs::RefError;
    let (status, code, message) = match error {
        RefError::Access(error) => return super::access::access_error(error),
        RefError::Git(error) => return repo_store_error(error, StatusCode::BAD_REQUEST),
        RefError::Invalid(message) => (StatusCode::BAD_REQUEST, "validation_error", message),
        RefError::Protected(reason) => (StatusCode::FORBIDDEN, "protected_branch", reason),
        RefError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "database_error", "Repository data is unavailable"),
        RefError::AppliedFollowupFailed => (StatusCode::SERVICE_UNAVAILABLE, "followup_failed", "The Git reference changed, but repository follow-up failed. Check the repository before retrying."),
    };
    (status, Json(ApiError::new(code, message)))
}

pub(crate) fn engagement_error(
    error: crate::backend::repositories::engagement::EngagementError,
) -> (StatusCode, Json<ApiError>) {
    use crate::backend::repositories::engagement::EngagementError;
    let message = match error {
        EngagementError::Access(error) => return super::access::access_error(error),
        EngagementError::Unavailable => "Repository operation is unavailable",
        EngagementError::ChangedCountUnavailable => {
            "The repository preference changed, but its updated count is unavailable"
        }
    };
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError::new("unavailable", message)),
    )
}

#[cfg(test)]
mod error_tests {
    use super::*;
    use crate::backend::repositories::store::{ErrorKind, RepoError};
    #[test]
    fn public_repository_failures_do_not_expose_internal_details() {
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::MissingPath,
            ErrorKind::Invalid,
            ErrorKind::Conflict,
            ErrorKind::Unavailable,
        ] {
            let (_, Json(body)) = repo_store_error(
                RepoError::new(kind, "private-path credential query"),
                StatusCode::BAD_REQUEST,
            );
            assert!(!body.message.contains("private-path"));
            assert!(!body.message.contains("credential"));
            assert!(!body.message.contains("query"));
        }
        let (status, Json(body)) = repo_store_error(
            RepoError::storage_node_unavailable(),
            StatusCode::BAD_REQUEST,
        );
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.message, "storage node unavailable");
        let (status, Json(body)) =
            repo_store_error(RepoError::response_too_large(), StatusCode::BAD_REQUEST);
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body.error, "response_too_large");
    }

    #[test]
    fn unreachable_storage_is_503_and_only_refused_refs_are_400() {
        use crate::backend::repositories::refs::RefError;
        let (status, _) = ref_error(RefError::Git(RepoError::authority_unavailable()));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let (status, _) = ref_error(RefError::Git(RepoError::new(
            ErrorKind::Invalid,
            "Invalid branch name",
        )));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = ref_error(RefError::Git(RepoError::new(
            ErrorKind::NotFound,
            "missing",
        )));
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

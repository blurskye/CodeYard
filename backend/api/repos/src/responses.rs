use crate::refs::commits::statuses::CommitStatusResponse;
use axum::http::StatusCode;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::shared::user::responses::PublicUserResponse as UserResponse;
use codeyard_git::CommitInfo;
use serde::Serialize;

/// Safe canonical namespace projection, never numeric-ID owner inference.
pub(crate) async fn repository_owner(
    pool: &codeyard_db_core::DbPool,
    repository: &codeyard_db_core::models::Repository,
) -> Result<String, (StatusCode, Json<ApiError>)> {
    let mut owners = repository_owners(pool, &[repository.id]).await?;
    Ok(owners.remove(&repository.id).expect("validated owner"))
}

pub(crate) async fn repository_owners(
    pool: &codeyard_db_core::DbPool,
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
    let owners = codeyard_db_repos::repositories::records::repository_owner_names(pool, ids)
        .await
        .map_err(|_| unavailable())?;
    if ids.iter().any(|id| !owners.contains_key(id)) {
        return Err(unavailable());
    }
    Ok(owners)
}

/// The topics of each listed repository. A repository without topics is absent from the map.
pub(crate) async fn repository_topics(
    pool: &codeyard_db_core::DbPool,
    ids: &[i64],
) -> Result<std::collections::BTreeMap<i64, Vec<String>>, (StatusCode, Json<ApiError>)> {
    codeyard_db_repos::repositories::topics::repository_topics(pool, ids)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new(
                    "unavailable",
                    "Repository topics are unavailable",
                )),
            )
        })
}

pub(crate) fn map_commit_status_response(
    status: codeyard_db_core::models::CommitStatus,
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
    pub website: Option<String>,
    pub topics: Vec<String>,
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

impl RepoResponse {
    /// The repository as the API lists it. `owner` is the namespace it belongs to.
    pub(crate) fn new(
        repository: &codeyard_db_core::models::Repository,
        owner: &str,
        topics: Vec<String>,
        forked_from: Option<ForkParent>,
        forks_count: i32,
    ) -> Self {
        Self {
            id: repository.id,
            owner_id: repository.owner_id(),
            full_name: format!("{owner}/{}", repository.name),
            owner: owner.to_owned(),
            name: repository.name.clone(),
            description: repository.description.clone(),
            website: repository.website.clone(),
            topics,
            is_private: repository.is_private,
            is_archived: repository.is_archived,
            allow_merge_commit: repository.allow_merge_commit,
            allow_squash_merge: repository.allow_squash_merge,
            allow_rebase_merge: repository.allow_rebase_merge,
            default_branch: repository.default_branch.clone(),
            stars_count: repository.stars_count,
            forks_count,
            watchers_count: repository.watchers_count,
            forked_from,
            created_at: repository.created_at.to_rfc3339(),
            updated_at: repository.updated_at.to_rfc3339(),
        }
    }
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
        let date = |timestamp| rfc3339(timestamp).unwrap_or_default();
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

pub fn repo_store_error(
    error: codeyard_git::error::RepoError,
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
        use codeyard_git::error::ErrorKind;
        let missing = matches!(error.kind, ErrorKind::NotFound | ErrorKind::MissingPath);
        (
            if missing {
                StatusCode::NOT_FOUND
            } else if error.kind == ErrorKind::Conflict {
                StatusCode::CONFLICT
            } else if error.kind == ErrorKind::Unavailable {
                // Unreachable storage is a service failure, whatever the caller falls back to.
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                fallback
            },
            "git_error",
            match error.kind {
                codeyard_git::error::ErrorKind::NotFound
                | codeyard_git::error::ErrorKind::MissingPath => "Requested Git resource not found",
                codeyard_git::error::ErrorKind::Invalid => "Invalid Git operation",
                codeyard_git::error::ErrorKind::Conflict => "Git reference changed",
                codeyard_git::error::ErrorKind::Unavailable => "Repository storage is unavailable",
            },
        )
    };
    (status, Json(ApiError::new(code, message)))
}

pub(crate) fn lifecycle_status(
    error: &codeyard_app::repositories::lifecycle::StorageError,
) -> StatusCode {
    use codeyard_app::repositories::lifecycle::LifecycleFailure;
    match error.kind {
        LifecycleFailure::Invalid | LifecycleFailure::OwnRepositoryFork => StatusCode::BAD_REQUEST,
        LifecycleFailure::Denied => StatusCode::FORBIDDEN,
        LifecycleFailure::Conflict => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// The answer when the database cannot be reached while handling a discussion.
pub(crate) fn storage_unavailable() -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError::new(
            "database_error",
            "Collaboration storage is temporarily unavailable",
        )),
    )
}

pub(crate) fn lifecycle_error(
    error: codeyard_app::repositories::lifecycle::StorageError,
) -> (StatusCode, Json<ApiError>) {
    use codeyard_app::repositories::lifecycle::LifecycleFailure;
    let code = match error.kind {
        LifecycleFailure::Invalid => "validation_error",
        LifecycleFailure::OwnRepositoryFork => "invalid_operation",
        LifecycleFailure::Denied => "forbidden",
        LifecycleFailure::Conflict => "conflict",
        LifecycleFailure::DeletedStorageRemains => "storage_unavailable",
        LifecycleFailure::Unavailable | LifecycleFailure::InitializationCleanupFailed => {
            "storage_pending"
        }
    };
    (
        lifecycle_status(&error),
        Json(ApiError::new(code, error.message)),
    )
}

pub(crate) fn ref_error(
    error: codeyard_app::repositories::refs::RefError,
) -> (StatusCode, Json<ApiError>) {
    use codeyard_app::repositories::refs::RefError;
    let (status, code, message) = match error {
        RefError::Access(error) => return codeyard_api_http::shared::repos::access::access_error(error),
        RefError::Git(error) => return repo_store_error(error, StatusCode::BAD_REQUEST),
        RefError::Invalid(message) => (StatusCode::BAD_REQUEST, "validation_error", message),
        RefError::Protected(reason) => (StatusCode::FORBIDDEN, "protected_branch", reason),
        RefError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "database_error", "Repository data is unavailable"),
        RefError::AppliedFollowupFailed => (StatusCode::SERVICE_UNAVAILABLE, "followup_failed", "The Git reference changed, but repository follow-up failed. Check the repository before retrying."),
    };
    (status, Json(ApiError::new(code, message)))
}

pub(crate) fn engagement_error(
    error: codeyard_app::repositories::engagement::EngagementError,
) -> (StatusCode, Json<ApiError>) {
    use codeyard_app::repositories::engagement::EngagementError;
    let message = match error {
        EngagementError::Access(error) => {
            return codeyard_api_http::shared::repos::access::access_error(error)
        }
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
    use codeyard_git::error::ErrorKind;
    use codeyard_git::error::RepoError;
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
        use codeyard_app::repositories::refs::RefError;
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

use codeyard_api_http::shared::repos::responses::rfc3339;

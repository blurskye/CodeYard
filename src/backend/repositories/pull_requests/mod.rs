use crate::backend::db;
use crate::backend::repositories::access;
pub mod branches;
pub mod code_owners;
pub mod comments;
pub mod merge;
pub mod mutations;
pub mod restack;
pub mod reviews;
pub mod stacks;
pub mod workflows;

pub use branches::{
    branches_moved, head_repository, refresh_open_pull_request, refresh_open_pull_requests,
};
pub use workflows::queue_pull_request_ci;

#[derive(Debug, thiserror::Error)]
pub enum OperationError {
    #[error("{message}")]
    Invalid { code: &'static str, message: String },
    #[error("{message}")]
    Missing { code: &'static str, message: String },
    #[error("{message}")]
    Denied { code: &'static str, message: String },
    #[error("{message}")]
    Unauthenticated { code: &'static str, message: String },
    #[error("{message}")]
    Conflict { code: &'static str, message: String },
    #[error("{message}")]
    Unavailable { code: &'static str, message: String },
    #[error("{message}")]
    TooLarge { code: &'static str, message: String },
    #[error("The branch was merged, but the pull request status could not be updated")]
    BranchMerged { sha: String },
    #[error("Pull request opened but its head could not be fetched")]
    Opened { number: i32 },
    #[error("The change was saved, but pull request revisions could not be refreshed")]
    Updated { number: i32 },
}

pub(super) async fn actor(pool: &db::DbPool, id: i64) -> Result<db::models::User, OperationError> {
    db::users::accounts::get_user_by_id(pool, id)
        .await
        .map_err(write_error)?
        .filter(|user| !user.is_banned)
        .ok_or_else(|| OperationError::Unauthenticated {
            code: "unauthorized",
            message: "Please sign in again".into(),
        })
}

async fn can_read(
    pool: &db::DbPool,
    user: &db::models::User,
    repo: &db::models::Repository,
) -> Result<bool, OperationError> {
    access::can_read(pool, Some(user), repo)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Repository authority is unavailable".into(),
        })
}

pub(super) async fn require_read(
    pool: &db::DbPool,
    user: &db::models::User,
    repo: &db::models::Repository,
) -> Result<(), OperationError> {
    if can_read(pool, user, repo).await? {
        Ok(())
    } else {
        Err(OperationError::Missing {
            code: "not_found",
            message: "Repository not found".into(),
        })
    }
}

pub(super) async fn require_write(
    pool: &db::DbPool,
    user: &db::models::User,
    repo: &db::models::Repository,
) -> Result<(), OperationError> {
    if access::can_write(pool, user, repo)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Repository authority is unavailable".into(),
        })?
    {
        Ok(())
    } else if (repo.is_archived || user.must_change_password) && can_read(pool, user, repo).await? {
        // Only someone who can read the repository learns why a write is refused.
        Err(OperationError::Denied {
            code: "write_denied",
            message: "Archived repositories and password-change-required accounts cannot write"
                .into(),
        })
    } else {
        Err(OperationError::Missing {
            code: "not_found",
            message: "Repository not found".into(),
        })
    }
}

pub(super) async fn require_admin(
    pool: &db::DbPool,
    user: &db::models::User,
    repo: &db::models::Repository,
) -> Result<(), OperationError> {
    if access::can_admin(pool, user, repo)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Repository authority is unavailable".into(),
        })?
    {
        Ok(())
    } else {
        Err(OperationError::Missing {
            code: "not_found",
            message: "Repository not found".into(),
        })
    }
}

pub(super) fn write_error(error: anyhow::Error) -> OperationError {
    use db::discussions::DiscussionWriteRefusal;
    match error.downcast_ref::<DiscussionWriteRefusal>() {
        Some(DiscussionWriteRefusal::Denied) => OperationError::Denied {
            code: "forbidden",
            message: "This discussion operation is not permitted".into(),
        },
        Some(DiscussionWriteRefusal::Locked) => OperationError::Denied {
            code: "conversation_locked",
            message: "This conversation is locked.".into(),
        },
        Some(DiscussionWriteRefusal::InvalidReview) => OperationError::Conflict {
            code: "conflict",
            message: "The pull request no longer accepts this review".into(),
        },
        Some(DiscussionWriteRefusal::RevisionChanged) => OperationError::Conflict {
            code: "revision_conflict",
            message: "The pull request revisions changed. Reload the diff before commenting."
                .into(),
        },
        Some(DiscussionWriteRefusal::ReviewFull) => OperationError::Invalid {
            code: "validation_error",
            message: "A review holds at most 200 comments.".into(),
        },
        None => OperationError::Unavailable {
            code: "database_error",
            message: "Discussion storage is unavailable; no success was confirmed".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_failures_discard_private_driver_context() {
        let error = write_error(anyhow::anyhow!(
            "postgres://secret:password@private/repository"
        ));
        assert!(matches!(error, OperationError::Unavailable { .. }));
        assert!(!error.to_string().contains("password"));
        assert!(!error.to_string().contains("private"));
    }

    #[test]
    fn committed_external_effects_are_distinct_failures() {
        let error = OperationError::BranchMerged {
            sha: "a".repeat(40),
        };
        assert!(error.to_string().contains("branch was merged"));
        assert!(matches!(error, OperationError::BranchMerged { sha } if sha == "a".repeat(40)));
        let error = OperationError::Opened { number: 12 };
        assert!(matches!(error, OperationError::Opened { number: 12 }));
    }
}

use crate::backend::db;
use crate::backend::repositories::access;
pub mod async_merge;
pub mod auto_merge;
pub mod branches;
pub mod code_owners;
pub mod comments;
pub(crate) mod maintainer_edit;
pub mod merge;
pub mod mutations;
pub mod restack;
pub mod revert;
pub mod reviews;
pub mod stacks;
pub mod team_reviewers;
pub mod update_branch;

pub mod workflows;

pub use branches::{
    branches_moved, head_repository, refresh_open_pull_request, refresh_open_pull_requests,
};
pub use workflows::queue_pull_request_ci;

/// The `pull_request` webhook payload with the diff and commit counts, which the pull request row
/// does not keep. It runs in a webhook builder, which is a blocking task.
pub(crate) fn webhook_payload(
    action: &str,
    pull: &codeyard_db_core::models::PullRequest,
    author: &crate::backend::webhooks::payloads::Person,
    scope: crate::backend::webhooks::payloads::Scope,
    store: &crate::backend::repositories::store::RepoStore,
) -> serde_json::Value {
    let mut payload = crate::backend::webhooks::payloads::pull_request(action, pull, author, scope);
    let base = pull.diff_base();
    let (diff, commits) = tokio::runtime::Handle::current().block_on(async {
        tokio::join!(
            store.diff_stats(base, &pull.head_sha),
            store.pull_request_commits(base, &pull.head_sha, 0, 0)
        )
    });
    let document = &mut payload["pull_request"];
    match diff {
        Ok(diff) => {
            document["additions"] = diff.additions.into();
            document["deletions"] = diff.deletions.into();
            document["changed_files"] = diff.changed_files.into();
        }
        Err(error) => {
            tracing::warn!(pull_id = pull.id, %error, "could not count pull request changes")
        }
    }
    match commits {
        Ok((count, _)) => document["commits"] = count.into(),
        Err(error) => {
            tracing::warn!(pull_id = pull.id, %error, "could not count pull request commits")
        }
    }
    payload
}

/// Sends one `pull_request` webhook for `pull` and returns its document, which is also the
/// workflow event document. `extra` adds top-level fields such as `changes` or
/// `requested_reviewer`. Without the cluster context a storage node's repository is sent
/// without its diff counts.
pub(crate) async fn emit_pull_request_event(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&crate::backend::cluster::NodeContext>,
    repository: &codeyard_db_core::models::Repository,
    pull: &codeyard_db_core::models::PullRequest,
    action: &'static str,
    sender_id: i64,
    extra: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    let (pull_for_hook, repository_for_hook) = (pull.clone(), repository.clone());
    let author_id = pull.author_id;
    let store = crate::backend::repositories::store::repo_store(repository, cluster);
    crate::backend::webhooks::emit(
        pool,
        repository,
        crate::backend::webhooks::events::PULL_REQUEST,
        vec![author_id, sender_id],
        move |owner, people| {
            let mut payload = webhook_payload(
                action,
                &pull_for_hook,
                &people.get(author_id),
                crate::backend::webhooks::payloads::Scope {
                    repo: &repository_for_hook,
                    owner,
                    sender: &people.get(sender_id),
                },
                &store,
            );
            if let Some(document) = payload.as_object_mut() {
                document.extend(extra);
            }
            payload
        },
    )
    .await
}

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

/// A storage failure, reported without the driver's own text.
fn database_error<E>(_: E) -> OperationError {
    OperationError::Unavailable {
        code: "database_error",
        message: "Collaboration storage is temporarily unavailable".into(),
    }
}

/// The repository a caller already resolved, read again by ID with its owner's name.
pub(super) async fn repository(
    pool: &codeyard_db_core::DbPool,
    id: i64,
) -> Result<(codeyard_db_core::models::Repository, String), OperationError> {
    use crate::backend::repositories::access::{current_repository_with_owner, AccessError};
    current_repository_with_owner(pool, id)
        .await
        .map_err(|error| match error {
            AccessError::Unavailable => database_error(()),
            _ => OperationError::Missing {
                code: "not_found",
                message: "Repository not found".to_owned(),
            },
        })
}

pub(super) async fn actor(
    pool: &codeyard_db_core::DbPool,
    id: i64,
) -> Result<codeyard_db_core::models::User, OperationError> {
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
    pool: &codeyard_db_core::DbPool,
    user: &codeyard_db_core::models::User,
    repo: &codeyard_db_core::models::Repository,
) -> Result<bool, OperationError> {
    access::can_read(pool, Some(user), repo)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Repository authority is unavailable".into(),
        })
}

pub(super) async fn require_read(
    pool: &codeyard_db_core::DbPool,
    user: &codeyard_db_core::models::User,
    repo: &codeyard_db_core::models::Repository,
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
    pool: &codeyard_db_core::DbPool,
    user: &codeyard_db_core::models::User,
    repo: &codeyard_db_core::models::Repository,
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
    pool: &codeyard_db_core::DbPool,
    user: &codeyard_db_core::models::User,
    repo: &codeyard_db_core::models::Repository,
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

pub(crate) fn write_error(error: anyhow::Error) -> OperationError {
    if error.is::<db::pull_caps::PullCapReached>() {
        return OperationError::Invalid {
            code: "pull_request_creation_cap",
            message: error.to_string(),
        };
    }
    use db::discussions::DiscussionWriteRefusal;
    if let Some(refusal) =
        error.downcast_ref::<db::pull_requests::records::IssueConversionRefusal>()
    {
        return OperationError::Invalid {
            code: "invalid_issue_conversion",
            message: refusal.to_string(),
        };
    }
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
        Some(DiscussionWriteRefusal::EmptyReview) => OperationError::Invalid {
            code: "validation_error",
            message: "A review body is required unless the review is approved".into(),
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

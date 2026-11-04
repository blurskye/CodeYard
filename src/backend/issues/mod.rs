pub mod assignments;
pub mod branches;
pub mod closing;
pub mod comments;
pub mod mutations;
pub mod references;
pub mod suggestions;

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
}

/// Comments on one discussion page. Notification and redirect links name the page that
/// holds a comment, so they must use the size the discussion page renders.
pub(crate) const COMMENTS_PER_PAGE: usize = 30;
/// The last discussion page a URL can name.
pub(crate) const MAX_COMMENT_PAGE: usize = (i64::MAX as usize) / COMMENTS_PER_PAGE;

/// The discussion page that holds the item at `position`, counted from 1 in the order the
/// discussion lists it. If the position cannot be read, the first page is named, so a successful
/// write does not turn into an error that invites a duplicate retry.
pub(crate) fn discussion_page(position: anyhow::Result<i64>) -> usize {
    match position {
        Ok(position) => (position as usize)
            .div_ceil(COMMENTS_PER_PAGE)
            .clamp(1, MAX_COMMENT_PAGE),
        Err(error) => {
            tracing::warn!(%error, "Saved discussion item has an unknown page");
            1
        }
    }
}

/// The largest `review_comments` offset a URL can name.
pub(crate) const MAX_REVIEW_COMMENT_OFFSET: usize = 1_000_000;

/// The link to `anchor` on discussion `page` of `base`.
pub(crate) fn discussion_link(base: &str, page: usize, anchor: &str) -> String {
    discussion_link_at(base, page, 0, anchor)
}

/// Like `discussion_link`, and keeps the `review_comments` offset of the reader's place inside
/// the reviews of that page. An empty `anchor` names no fragment.
pub(crate) fn discussion_link_at(
    base: &str,
    page: usize,
    review_comments: usize,
    anchor: &str,
) -> String {
    let mut query = Vec::new();
    if page > 1 {
        query.push(format!("page={page}"));
    }
    if review_comments > 0 {
        query.push(format!("review_comments={review_comments}"));
    }
    let mut link = base.to_owned();
    if !query.is_empty() {
        link = format!("{link}?{}", query.join("&"));
    }
    if !anchor.is_empty() {
        link = format!("{link}#{anchor}");
    }
    link
}

/// The discussion page and anchor of a comment that is already saved.
pub(crate) fn comment_link(base: &str, id: i64, position: anyhow::Result<i64>) -> String {
    discussion_link(base, discussion_page(position), &format!("comment-{id}"))
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
    use crate::backend::repositories::access::current_repository_with_owner;
    use crate::backend::repositories::access::AccessError;
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
    crate::backend::db::users::accounts::get_user_by_id(pool, id)
        .await
        .map_err(write_error)?
        .filter(|user| !user.is_banned)
        .ok_or_else(|| OperationError::Unauthenticated {
            code: "unauthorized",
            message: "Please sign in again".into(),
        })
}

/// Repository access decisions belong to `repositories::access`; this only says them in the
/// terms of these operations. Only someone who can read the repository learns why a write is
/// refused.
fn access_error(error: crate::backend::repositories::access::AccessError) -> OperationError {
    match error {
        crate::backend::repositories::access::AccessError::Unavailable => {
            OperationError::Unavailable {
                code: "database_error",
                message: "Repository authority is unavailable".into(),
            }
        }
        crate::backend::repositories::access::AccessError::WriteDenied
        | crate::backend::repositories::access::AccessError::PasswordChangeRequired => {
            OperationError::Denied {
                code: "write_denied",
                message: "Archived repositories and password-change-required accounts cannot write"
                    .into(),
            }
        }
        crate::backend::repositories::access::AccessError::NotFound
        | crate::backend::repositories::access::AccessError::InvalidCollaboratorOwner => {
            OperationError::Missing {
                code: "not_found",
                message: "Repository not found".into(),
            }
        }
    }
}

pub(super) async fn require_read(
    pool: &codeyard_db_core::DbPool,
    user: &codeyard_db_core::models::User,
    repo: &codeyard_db_core::models::Repository,
) -> Result<(), OperationError> {
    crate::backend::repositories::access::check_read(pool, repo.id, Some(user.id))
        .await
        .map(|_| ())
        .map_err(access_error)
}

pub(super) async fn require_write(
    pool: &codeyard_db_core::DbPool,
    user: &codeyard_db_core::models::User,
    repo: &codeyard_db_core::models::Repository,
) -> Result<(), OperationError> {
    crate::backend::repositories::access::check_write(pool, repo.id, user.id)
        .await
        .map(|_| ())
        .map_err(access_error)
}

pub(super) fn write_error(error: anyhow::Error) -> OperationError {
    use crate::backend::db::discussions::DiscussionWriteRefusal;
    if error.is::<codeyard_support::credentials::AccountRestricted>() {
        return OperationError::Denied {
            code: "forbidden",
            message: "A live unrestricted credential is required.".into(),
        };
    }
    // Initial metadata now shares the create transaction, so preserve its typed refusals.
    let metadata = error
        .downcast_ref::<crate::backend::db::assignees::AssignmentRefusal>()
        .map(|r| {
            (
                matches!(r, crate::backend::db::assignees::AssignmentRefusal::Denied),
                matches!(
                    r,
                    crate::backend::db::assignees::AssignmentRefusal::NotFound
                ),
                r.to_string(),
            )
        })
        .or_else(|| {
            error
                .downcast_ref::<crate::backend::db::labels::LabelWriteRefusal>()
                .map(|r| {
                    (
                        matches!(r, crate::backend::db::labels::LabelWriteRefusal::Denied),
                        false,
                        r.to_string(),
                    )
                })
        })
        .or_else(|| {
            error
                .downcast_ref::<crate::backend::db::milestones::MilestoneWriteRefusal>()
                .map(|r| {
                    (
                        matches!(
                            r,
                            crate::backend::db::milestones::MilestoneWriteRefusal::Denied
                        ),
                        matches!(
                            r,
                            crate::backend::db::milestones::MilestoneWriteRefusal::NotFound
                        ),
                        r.to_string(),
                    )
                })
        })
        .or_else(|| {
            error
                .downcast_ref::<crate::backend::db::issue_attributes::Refusal>()
                .map(|r| {
                    (
                        matches!(r, crate::backend::db::issue_attributes::Refusal::Denied),
                        matches!(r, crate::backend::db::issue_attributes::Refusal::Missing),
                        r.to_string(),
                    )
                })
        });
    if let Some((denied, missing, message)) = metadata {
        return if denied {
            OperationError::Denied {
                code: "forbidden",
                message,
            }
        } else if missing {
            OperationError::Missing {
                code: "not_found",
                message,
            }
        } else {
            OperationError::Invalid {
                code: "validation_error",
                message,
            }
        };
    }
    if let Some(refusal) = error.downcast_ref::<crate::backend::db::relationships::Refusal>() {
        return match refusal {
            crate::backend::db::relationships::Refusal::Denied => OperationError::Denied {
                code: "forbidden",
                message: refusal.to_string(),
            },
            crate::backend::db::relationships::Refusal::Missing => OperationError::Missing {
                code: "not_found",
                message: refusal.to_string(),
            },
            _ => OperationError::Invalid {
                code: "validation_error",
                message: refusal.to_string(),
            },
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
        Some(DiscussionWriteRefusal::RevisionChanged) => OperationError::Conflict {
            code: "revision_conflict",
            message: "The discussion revision changed".into(),
        },
        Some(DiscussionWriteRefusal::InvalidReview) => OperationError::Conflict {
            code: "conflict",
            message: "The pull request no longer accepts this review".into(),
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
    use codeyard_model::issue_input::validate_body;
    use codeyard_model::issue_input::validate_title;

    #[test]
    fn blank_and_oversized_text_is_refused_in_bytes() {
        assert!(validate_title(" \n").is_err());
        assert!(validate_title(&"x".repeat(256)).is_err());
        assert!(validate_title(&"é".repeat(128)).is_err());
        assert!(validate_title(&"x".repeat(255)).is_ok());
        assert!(validate_body(" \n", true).is_err());
        assert!(validate_body("", false).is_ok());
        assert!(validate_body(&"x".repeat(65_537), false).is_err());
        assert!(validate_body(&"x".repeat(65_536), false).is_ok());
    }

    #[test]
    fn label_descriptions_are_bounded_in_bytes() {
        assert!(crate::backend::issues::validate_label("label", Some(&"x".repeat(1024))).is_ok());
        assert!(crate::backend::issues::validate_label("label", Some(&"x".repeat(1025))).is_err());
    }

    #[test]
    fn comment_links_reach_pages_beyond_one_thousand() {
        assert_eq!(crate::backend::issues::discussion_page(Ok(30_001)), 1001);
        assert_eq!(
            crate::backend::issues::comment_link("/a/r/issues/1", 9, Ok(30_001)),
            "/a/r/issues/1?page=1001#comment-9"
        );
    }

    #[test]
    fn nul_text_is_refused() {
        assert!(validate_title("a\0b").is_err());
        assert!(validate_body("a\0b", false).is_err());
        assert!(validate_body("\0", true).is_err());
        assert!(validate_body("a\u{e9}b", false).is_ok());
    }
}

use codeyard_model::issue_input::validate_body;

use codeyard_model::issue_input::validate_label;

use codeyard_model::issue_input::validate_title;

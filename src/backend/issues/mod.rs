use crate::backend::db;
use crate::backend::repositories::access;
pub mod comments;
pub mod mutations;

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

/// Text limits for issues, pull requests, comments, reviews and labels. The JSON API and
/// the native forms both call these, so they refuse the same text. NUL is refused because
/// PostgreSQL text cannot store it, and a SQLite database holding it could not be copied.
pub fn validate_title(title: &str) -> Result<(), &'static str> {
    if title.trim().is_empty() || title.len() > 255 {
        Err("Title must contain 1–255 bytes of text.")
    } else if title.contains('\0') {
        Err(NUL_REFUSAL)
    } else {
        Ok(())
    }
}

pub fn validate_body(body: &str, required: bool) -> Result<(), &'static str> {
    if required && body.trim().is_empty() {
        return Err("Comment cannot be empty.");
    }
    if body.len() > 65_536 {
        return Err("Text cannot exceed 65,536 bytes.");
    }
    if body.contains('\0') {
        return Err(NUL_REFUSAL);
    }
    Ok(())
}

/// Label names are 1–50 bytes and descriptions at most 1,024 bytes. Both refuse NUL.
pub fn validate_label(name: &str, description: Option<&str>) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > 50 {
        Err("Label name must be 1-50 characters")
    } else if description.is_some_and(|text| text.len() > 1_024) {
        Err("Label description must be at most 1024 bytes")
    } else if name.contains('\0') || description.is_some_and(|text| text.contains('\0')) {
        Err(NUL_REFUSAL)
    } else {
        Ok(())
    }
}

const NUL_REFUSAL: &str = "Text cannot contain NUL characters.";

/// Milestone titles are 1–255 bytes. The description has the body limits.
pub fn validate_milestone(title: &str, description: &str) -> Result<(), &'static str> {
    if title.trim().is_empty() || title.len() > 255 {
        Err("Milestone title must contain 1–255 bytes of text.")
    } else if title.contains('\0') {
        Err(NUL_REFUSAL)
    } else {
        validate_body(description, false)
    }
}

/// A due date is a calendar day written YYYY-MM-DD. An empty value means no due date.
pub fn parse_due_date(value: &str) -> Result<Option<String>, &'static str> {
    if value.is_empty() {
        return Ok(None);
    }
    let day = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .filter(|day| day.format("%Y-%m-%d").to_string() == value);
    day.map(|day| Some(day.format("%Y-%m-%d").to_string()))
        .ok_or("Due date must be a day written YYYY-MM-DD.")
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

/// The link to `anchor` on discussion `page` of `base`.
pub(crate) fn discussion_link(base: &str, page: usize, anchor: &str) -> String {
    if page == 1 {
        format!("{base}#{anchor}")
    } else {
        format!("{base}?page={page}#{anchor}")
    }
}

/// The discussion page and anchor of a comment that is already saved.
pub(crate) fn comment_link(base: &str, id: i64, position: anyhow::Result<i64>) -> String {
    discussion_link(base, discussion_page(position), &format!("comment-{id}"))
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

pub(super) async fn require_read(
    pool: &db::DbPool,
    user: &db::models::User,
    repo: &db::models::Repository,
) -> Result<(), OperationError> {
    if access::can_read(pool, Some(user), repo)
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
    } else if repo.is_archived || user.must_change_password {
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
    use super::{validate_body, validate_title};

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
        assert!(super::validate_label("label", Some(&"x".repeat(1024))).is_ok());
        assert!(super::validate_label("label", Some(&"x".repeat(1025))).is_err());
    }

    #[test]
    fn comment_links_reach_pages_beyond_one_thousand() {
        assert_eq!(super::discussion_page(Ok(30_001)), 1001);
        assert_eq!(
            super::comment_link("/a/r/issues/1", 9, Ok(30_001)),
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

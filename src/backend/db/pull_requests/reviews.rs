//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{PullRequestComment, PullRequestReview, User};
use crate::backend::db::DbPool;
use anyhow::Result;

pub const MAX_COMMENT_PATH_BYTES: usize = 4 * 1024;

pub struct NewPullRequestComment<'a> {
    pub pr_id: i64,
    pub author_id: i64,
    pub body: &'a str,
    pub path: Option<&'a str>,
    pub line: Option<i32>,
    pub side: Option<&'a str>,
    pub commit_sha: Option<&'a str>,
    pub in_reply_to_comment_id: Option<i64>,
}

native_operation!(crate::backend::db::postgres::pull_requests::reviews::add_pull_request_comment,
    crate::backend::db::sqlite::pull_requests::reviews::add_pull_request_comment,
    fn add_pull_request_comment(
    pool: &DbPool,
    input: NewPullRequestComment<'_>,
) -> Result<PullRequestComment>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_pull_request_comments,
    crate::backend::db::sqlite::pull_requests::reviews::list_pull_request_comments,
    fn list_pull_request_comments(
    pool: &DbPool,
    pr_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<PullRequestCommentEntry>>);

/// Inline comments on files are listed up to this many; `truncated` says more exist.
pub const MAX_INLINE_COMMENTS: usize = 500;

native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_pull_request_inline_comments,
    crate::backend::db::sqlite::pull_requests::reviews::list_pull_request_inline_comments,
    fn list_pull_request_inline_comments(pool: &DbPool, pr_id: i64) -> Result<PullRequestInlineComments>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::count_pull_request_comments,
    crate::backend::db::sqlite::pull_requests::reviews::count_pull_request_comments,
    fn count_pull_request_comments(pool: &DbPool, pr_id: i64) -> Result<i64>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::get_pull_request_comment_by_id,
    crate::backend::db::sqlite::pull_requests::reviews::get_pull_request_comment_by_id,
    fn get_pull_request_comment_by_id(
    pool: &DbPool,
    comment_id: i64,
) -> Result<Option<PullRequestComment>>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::count_unresolved_conversations,
    crate::backend::db::sqlite::pull_requests::reviews::count_unresolved_conversations,
    fn count_unresolved_conversations(pool: &DbPool, pr_id: i64) -> Result<i64>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::set_pull_request_comment_resolved,
    crate::backend::db::sqlite::pull_requests::reviews::set_pull_request_comment_resolved,
    fn set_pull_request_comment_resolved(
    pool: &DbPool,
    pr_id: i64,
    comment_id: i64,
    resolved: bool,
    resolver_id: i64,
) -> Result<Option<PullRequestComment>>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::add_pull_request_review,
    crate::backend::db::sqlite::pull_requests::reviews::add_pull_request_review,
    fn add_pull_request_review(
    pool: &DbPool,
    pr_id: i64,
    author_id: i64,
    state: &str,
    body: Option<&str>,
    commit_sha: Option<&str>,
) -> Result<PullRequestReview>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_pull_request_reviews,
    crate::backend::db::sqlite::pull_requests::reviews::list_pull_request_reviews,
    fn list_pull_request_reviews(
    pool: &DbPool,
    pr_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<PullRequestReviewEntry>>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_pull_request_review_decisions,
    crate::backend::db::sqlite::pull_requests::reviews::list_pull_request_review_decisions,
    fn list_pull_request_review_decisions(
    pool: &DbPool,
    pr_id: i64,
) -> Result<Vec<PullRequestReviewDecision>>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::get_pull_request_review_by_id,
    crate::backend::db::sqlite::pull_requests::reviews::get_pull_request_review_by_id,
    fn get_pull_request_review_by_id(
    pool: &DbPool,
    review_id: i64,
) -> Result<Option<PullRequestReview>>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::dismiss_pull_request_review,
    crate::backend::db::sqlite::pull_requests::reviews::dismiss_pull_request_review,
    fn dismiss_pull_request_review(
    pool: &DbPool,
    review_id: i64,
    dismissed_by_id: i64,
    reason: Option<&str>,
) -> Result<PullRequestReview>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::request_pull_request_reviewer,
    crate::backend::db::sqlite::pull_requests::reviews::request_pull_request_reviewer,
    fn request_pull_request_reviewer(
    pool: &DbPool,
    pr_id: i64,
    actor_id: i64,
    user_id: i64,
) -> Result<bool>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::remove_pull_request_reviewer,
    crate::backend::db::sqlite::pull_requests::reviews::remove_pull_request_reviewer,
    fn remove_pull_request_reviewer(pool: &DbPool, pr_id: i64, actor_id: i64, user_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_pull_request_reviewers,
    crate::backend::db::sqlite::pull_requests::reviews::list_pull_request_reviewers,
    fn list_pull_request_reviewers(pool: &DbPool, pr_id: i64) -> Result<Vec<User>>);

#[derive(Debug, Clone)]
pub struct PullRequestCommentEntry {
    pub comment: PullRequestComment,
    pub author: Option<crate::backend::db::users::accounts::AuthorDisplay>,
}

#[derive(Debug, Clone)]
pub struct PullRequestInlineComments {
    pub comments: Vec<PullRequestCommentEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct PullRequestReviewEntry {
    pub review: PullRequestReview,
    pub author: Option<crate::backend::db::users::accounts::AuthorDisplay>,
}

/// A reviewer's latest review that is not a comment or pending. Comments never
/// replace a vote; a dismissal does, so a dismissed review counts for nothing.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PullRequestReviewDecision {
    pub author_id: i64,
    pub state: String,
    pub commit_sha: Option<String>,
    /// `None` when the account no longer exists.
    pub username: Option<String>,
}

//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{PullRequestComment, PullRequestReview, User};
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

pub const MAX_COMMENT_PATH_BYTES: usize = 4 * 1024;

/// A pending review holds at most this many comments. Published thread replies do not use this
/// limit. Reads paginate comments separately from their reviews.
pub const MAX_REVIEW_COMMENTS: i64 = 200;

native_operation!(crate::backend::db::postgres::pull_requests::reviews::converted_issue_comment,
    crate::backend::db::sqlite::pull_requests::reviews::converted_issue_comment,
    fn converted_issue_comment(pool: &DbPool, pr_id: i64, source_comment: i64) -> Result<Option<i64>>);

pub struct NewPullRequestComment<'a> {
    pub pr_id: i64,
    pub author_id: i64,
    pub body: &'a str,
    pub path: Option<&'a str>,
    pub line: Option<i32>,
    pub side: Option<&'a str>,
    pub start_line: Option<i32>,
    pub start_side: Option<&'a str>,
    pub commit_sha: Option<&'a str>,
    pub in_reply_to_comment_id: Option<i64>,
    /// The diff validated for a new inline thread, as (head, base).
    pub expected_revisions: Option<(&'a str, &'a str)>,
}

native_operation!(crate::backend::db::postgres::pull_requests::reviews::add_pull_request_comment,
    crate::backend::db::sqlite::pull_requests::reviews::add_pull_request_comment,
    fn add_pull_request_comment(
    pool: &DbPool,
    input: NewPullRequestComment<'_>,
) -> Result<PullRequestComment>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::add_comment_to_pending_review,
    crate::backend::db::sqlite::pull_requests::reviews::add_comment_to_pending_review,
    fn add_comment_to_pending_review(pool: &DbPool, input: NewPullRequestComment<'_>, review_id: i64) -> Result<PullRequestComment>);

// Files a comment in the author's pending review, which is started if there is none. The comment
// stays private to the author until the review is submitted.
native_operation!(crate::backend::db::postgres::pull_requests::reviews::add_pending_pull_request_comment,
    crate::backend::db::sqlite::pull_requests::reviews::add_pending_pull_request_comment,
    fn add_pending_pull_request_comment(
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
pub const MAX_REVIEW_COMMENTS_PAGE: usize = 100;
pub const MAX_REVIEW_COMMENT_PAGE_BYTES: usize = 1024 * 1024;

// The comments before this one, and the page bytes through it, within these reviews.
native_operation!(crate::backend::db::postgres::pull_requests::reviews::review_comment_position,
    crate::backend::db::sqlite::pull_requests::reviews::review_comment_position,
    fn review_comment_position(pool: &DbPool, review_ids: &[i64], comment_id: i64) -> Result<(i64, i64)>);

pub struct ReviewCommentPage {
    pub comments: Vec<PullRequestCommentEntry>,
    pub has_more: bool,
}

native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_review_comments_page,
    crate::backend::db::sqlite::pull_requests::reviews::list_review_comments_page,
    fn list_review_comments_page(pool: &DbPool, review_ids: &[i64], offset: i64) -> Result<ReviewCommentPage>);

// Inline comments that are not in a pending review, oldest first.
native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_pull_request_inline_comments,
    crate::backend::db::sqlite::pull_requests::reviews::list_pull_request_inline_comments,
    fn list_pull_request_inline_comments(pool: &DbPool, pr_id: i64) -> Result<PullRequestInlineComments>);

// The comments of the author's pending review on a pull request, oldest first.
native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_pending_review_comments,
    crate::backend::db::sqlite::pull_requests::reviews::list_pending_review_comments,
    fn list_pending_review_comments(
    pool: &DbPool,
    pr_id: i64,
    author_id: i64,
) -> Result<Vec<PullRequestCommentEntry>>);

// The comments filed in any of `review_ids`, oldest first.
native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_review_comments,
    crate::backend::db::sqlite::pull_requests::reviews::list_review_comments,
    fn list_review_comments(pool: &DbPool, review_ids: &[i64]) -> Result<Vec<PullRequestCommentEntry>>);

// The Conversation's items from `offset`, oldest first. A comment filed in a review is not an item
// of its own: it is shown with its review. Pending reviews are left out.
native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_discussion_items,
    crate::backend::db::sqlite::pull_requests::reviews::list_discussion_items,
    fn list_discussion_items(pool: &DbPool, pr_id: i64, limit: i64, offset: i64) -> Result<Vec<DiscussionItem>>);

// How many Conversation items come up to and including `item`, in the order
// `list_discussion_items` uses. Its page is the number divided up by the page size.
native_operation!(crate::backend::db::postgres::pull_requests::reviews::count_discussion_items_through,
    crate::backend::db::sqlite::pull_requests::reviews::count_discussion_items_through,
    fn count_discussion_items_through(pool: &DbPool, pr_id: i64, item: DiscussionRef) -> Result<i64>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::get_pending_review,
    crate::backend::db::sqlite::pull_requests::reviews::get_pending_review,
    fn get_pending_review(
    pool: &DbPool,
    pr_id: i64,
    author_id: i64,
) -> Result<Option<PullRequestReview>>);

// Deletes the author's pending review with its comments. False when there is none.
native_operation!(crate::backend::db::postgres::pull_requests::reviews::discard_pending_review,
    crate::backend::db::sqlite::pull_requests::reviews::discard_pending_review,
    fn discard_pending_review(
    pool: &DbPool,
    pr_id: i64,
    author_id: i64,
) -> Result<bool>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::pull_request_comment_counts,
    crate::backend::db::sqlite::pull_requests::reviews::pull_request_comment_counts,
    fn pull_request_comment_counts(pool: &DbPool, pr_id: i64) -> Result<(i64, i64)>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_pull_request_conversation_comments,
    crate::backend::db::sqlite::pull_requests::reviews::list_pull_request_conversation_comments,
    fn list_pull_request_conversation_comments(pool: &DbPool, pr_id: i64, limit: i64, offset: i64) -> Result<Vec<PullRequestCommentEntry>>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::count_pull_request_conversation_comments,
    crate::backend::db::sqlite::pull_requests::reviews::count_pull_request_conversation_comments,
    fn count_pull_request_conversation_comments(pool: &DbPool, pr_id: i64) -> Result<i64>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::count_pull_request_comments,
    crate::backend::db::sqlite::pull_requests::reviews::count_pull_request_comments,
    fn count_pull_request_comments(pool: &DbPool, pr_id: i64) -> Result<i64>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::count_pull_request_comments_for,
    crate::backend::db::sqlite::pull_requests::reviews::count_pull_request_comments_for,
    fn count_pull_request_comments_for(pool: &DbPool, pr_ids: &[i64]) -> Result<Vec<(i64, i64)>>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::get_visible_pull_request_comment,
    crate::backend::db::sqlite::pull_requests::reviews::get_visible_pull_request_comment,
    /// The comment as `viewer` may read it, in one statement: a comment in a pending review is
    /// visible only to that review's author, and one whose review is gone is not visible.
    fn get_visible_pull_request_comment(
    pool: &DbPool,
    comment_id: i64,
    viewer: i64,
) -> Result<Option<PullRequestComment>>);

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

// Changes the body of the author's conversation comment. None when the change is refused.
native_operation!(crate::backend::db::postgres::pull_requests::reviews::edit_pull_request_comment,
    crate::backend::db::sqlite::pull_requests::reviews::edit_pull_request_comment,
    fn edit_pull_request_comment(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    pr_id: i64,
    comment_id: i64,
    body: &str,
    expected_body: &str,
) -> Result<Option<PullRequestComment>>);

// Deletes a conversation comment: its author, or a maintainer. False when refused.
native_operation!(crate::backend::db::postgres::pull_requests::reviews::delete_pull_request_comment,
    crate::backend::db::sqlite::pull_requests::reviews::delete_pull_request_comment,
    fn delete_pull_request_comment(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    pr_id: i64,
    comment_id: i64,
) -> Result<bool>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::create_pending_review,
    crate::backend::db::sqlite::pull_requests::reviews::create_pending_review,
    fn create_pending_review(pool: &DbPool, pr_id: i64, author_id: i64, body: Option<&str>, commit_sha: &str) -> Result<PullRequestReview>);
native_operation!(crate::backend::db::postgres::pull_requests::reviews::edit_review_body,
    crate::backend::db::sqlite::pull_requests::reviews::edit_review_body,
    fn edit_review_body(pool: &DbPool, pr_id: i64, author_id: i64, review_id: i64, body: &str) -> Result<Option<PullRequestReview>>);
native_operation!(crate::backend::db::postgres::pull_requests::reviews::delete_pending_review_by_id,
    crate::backend::db::sqlite::pull_requests::reviews::delete_pending_review_by_id,
    fn delete_pending_review_by_id(pool: &DbPool, pr_id: i64, author_id: i64, review_id: i64) -> Result<Option<PullRequestReview>>);
native_operation!(crate::backend::db::postgres::pull_requests::reviews::save_pull_request_review,
    crate::backend::db::sqlite::pull_requests::reviews::save_pull_request_review,
    fn save_pull_request_review(pool: &DbPool, pr_id: i64, author_id: i64, state: &str, body: Option<&str>, commit_sha: Option<&str>, expected_pending: Option<i64>) -> Result<PullRequestReview>);

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

pub struct ReviewCommentFilter {
    pub repo_id: i64,
    pub pull_id: Option<i64>,
    pub review_id: Option<i64>,
    pub viewer: Option<i64>,
    pub updated: bool,
    pub ascending: bool,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub limit: i64,
    pub offset: i64,
}

native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_visible_review_comments,
    crate::backend::db::sqlite::pull_requests::reviews::list_visible_review_comments,
    fn list_visible_review_comments(pool: &DbPool, filter: &ReviewCommentFilter) -> Result<Vec<PullRequestComment>>);

#[derive(Default)]
pub struct ReviewQuery {
    pub pull_id: i64,
    pub viewer: Option<i64>,
    pub author: Option<String>,
    pub states: Vec<String>,
    pub latest: bool,
}
impl ReviewQuery {
    pub(crate) fn sql(postgres: bool, count: bool) -> String {
        let select = if count { "COUNT(*)" } else { "v.*" };
        let array = if postgres {
            "jsonb_array_elements_text(CAST($3 AS JSONB))"
        } else {
            "json_each($3)"
        };
        let length = if postgres {
            "jsonb_array_length(CAST($3 AS JSONB))"
        } else {
            "json_array_length($3)"
        };
        let page = if count {
            ""
        } else {
            " ORDER BY v.created_at,v.id LIMIT $6 OFFSET $7"
        };
        format!("SELECT {select} FROM pr_reviews v JOIN users u ON u.id=v.author_id WHERE v.pull_request_id=$1 AND (v.state<>'pending' OR v.author_id=CAST($2 AS BIGINT)) AND ({length}=0 OR v.state IN (SELECT value FROM {array} AS value)) AND (CAST($4 AS TEXT) IS NULL OR LOWER(u.username)=LOWER($4)) AND (NOT $5 OR (v.state<>'pending' AND NOT EXISTS(SELECT 1 FROM pr_reviews newer WHERE newer.pull_request_id=v.pull_request_id AND newer.author_id=v.author_id AND newer.state<>'pending' AND (newer.created_at>v.created_at OR (newer.created_at=v.created_at AND newer.id>v.id))))){page}")
    }
}
native_operation!(crate::backend::db::postgres::pull_requests::reviews::filter_reviews,
    crate::backend::db::sqlite::pull_requests::reviews::filter_reviews,
    fn filter_reviews(pool:&DbPool,query:&ReviewQuery,limit:i64,offset:i64)->Result<Vec<PullRequestReview>>);
native_operation!(crate::backend::db::postgres::pull_requests::reviews::count_filtered_reviews,
    crate::backend::db::sqlite::pull_requests::reviews::count_filtered_reviews,
    fn count_filtered_reviews(pool:&DbPool,query:&ReviewQuery)->Result<i64>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_visible_pull_request_reviews,
    crate::backend::db::sqlite::pull_requests::reviews::list_visible_pull_request_reviews,
    fn list_visible_pull_request_reviews(pool: &DbPool, pr_id: i64, viewer: Option<i64>, limit: i64, offset: i64) -> Result<Vec<PullRequestReview>>);

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

native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_pull_request_review_states,
    crate::backend::db::sqlite::pull_requests::reviews::list_pull_request_review_states,
    fn list_pull_request_review_states(pool: &DbPool, pr_ids: &[i64]) -> Result<Vec<(i64, String, Option<String>)>>);

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

// Internal callers supply owners derived from the recorded base's CODEOWNERS, never user input.
native_operation!(crate::backend::db::postgres::pull_requests::reviews::request_code_owner_reviews,
    crate::backend::db::sqlite::pull_requests::reviews::request_code_owner_reviews,
    fn request_code_owner_reviews(pool: &DbPool, pull: &crate::backend::db::models::PullRequest, actor_id: i64, owners: &[i64]) -> Result<Vec<User>>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::remove_pull_request_reviewer,
    crate::backend::db::sqlite::pull_requests::reviews::remove_pull_request_reviewer,
    fn remove_pull_request_reviewer(pool: &DbPool, pr_id: i64, actor_id: i64, user_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::pull_requests::reviews::list_pull_request_reviewers,
    crate::backend::db::sqlite::pull_requests::reviews::list_pull_request_reviewers,
    fn list_pull_request_reviewers(pool: &DbPool, pr_id: i64) -> Result<Vec<User>>);

#[derive(sqlx::FromRow)]
pub struct ReviewRequestOrigin {
    pub user_id: Option<i64>,
    pub team_id: Option<i64>,
    pub as_code_owner: bool,
}

native_operation!(crate::backend::db::postgres::pull_requests::reviews::review_request_origins,
    crate::backend::db::sqlite::pull_requests::reviews::review_request_origins,
    fn review_request_origins(pool: &DbPool, pull: i64) -> Result<Vec<ReviewRequestOrigin>>);

#[derive(Debug, Clone)]
pub struct PullRequestCommentEntry {
    pub comment: PullRequestComment,
    pub author: crate::backend::db::users::accounts::AuthorDisplay,
}

#[derive(Debug, Clone)]
pub struct PullRequestInlineComments {
    pub comments: Vec<PullRequestCommentEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct PullRequestReviewEntry {
    pub review: PullRequestReview,
    pub author: crate::backend::db::users::accounts::AuthorDisplay,
}

/// One item of a pull request's Conversation: a comment outside any review, or a submitted review.
#[derive(Debug, Clone)]
pub enum DiscussionItem {
    Comment(PullRequestCommentEntry),
    Review(PullRequestReviewEntry),
}

impl DiscussionItem {
    pub fn created_at(&self) -> DateTime<Utc> {
        match self {
            Self::Comment(entry) => entry.comment.created_at,
            Self::Review(entry) => entry.review.created_at,
        }
    }
}

/// One item of the Conversation, named by its kind and id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscussionRef {
    Comment(i64),
    Review(i64),
}

impl DiscussionRef {
    /// The item a comment is shown as: its review when it has one, otherwise the comment itself.
    pub fn of_comment(comment: &PullRequestComment) -> Self {
        match comment.review_id {
            Some(review_id) => Self::Review(review_id),
            None => Self::Comment(comment.id),
        }
    }
}

/// A reviewer's latest review that is not a comment or pending. Comments never
/// replace a vote; a dismissal does, so a dismissed review counts for nothing.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PullRequestReviewDecision {
    pub author_id: i64,
    pub state: String,
    pub commit_sha: Option<String>,
    pub username: String,
}

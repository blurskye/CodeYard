use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct PullRequest {
    pub id: i64,
    pub repo_id: i64,
    pub number: i32,
    pub author_id: i64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub head_repo_id: Option<i64>,
    pub workflows_approved_sha: Option<String>,
    /// The stack this pull request belongs to: the id of its bottom pull request.
    pub stack_id: Option<i64>,
    pub milestone_id: Option<i64>,
    pub head_branch: String,
    pub head_sha: String,
    pub base_branch: String,
    pub base_sha: String,
    pub is_draft: bool,
    pub mergeable: Option<bool>,
    pub merged: bool,
    pub merged_by_id: Option<i64>,
    pub merge_commit_sha: Option<String>,
    pub additions: i32,
    pub deletions: i32,
    pub changed_files: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub merged_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct PullRequestComment {
    pub id: i64,
    pub pull_request_id: i64,
    pub author_id: i64,
    pub body: String,
    pub path: Option<String>,
    pub line: Option<i32>,
    pub side: Option<String>,
    pub commit_sha: Option<String>,
    pub in_reply_to_comment_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Set on a review conversation (a root comment on a file) that someone resolved.
    pub resolved_at: Option<DateTime<Utc>>,
    pub resolved_by_id: Option<i64>,
    /// The review the comment was filed in; None for a comment posted on its own.
    pub review_id: Option<i64>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct PullRequestReview {
    pub id: i64,
    pub pull_request_id: i64,
    pub author_id: i64,
    pub state: String,
    pub body: Option<String>,
    pub commit_sha: Option<String>,
    pub dismissed_by_id: Option<i64>,
    pub dismissed_reason: Option<String>,
    pub dismissed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CommitStatus {
    pub id: i64,
    pub repo_id: i64,
    pub sha: String,
    pub context: String,
    pub state: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
    pub creator_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

//! Emoji reactions on issues, pull requests, their comments and their reviews.
use crate::backend::db::DbPool;
use anyhow::Result;

/// The reactions a user can pick, in display order.
pub const CONTENTS: [&str; 8] = [
    "+1", "-1", "laugh", "hooray", "confused", "heart", "rocket", "eyes",
];

/// What a reaction is attached to. The names are the values stored in `subject_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Issue,
    IssueComment,
    PullRequest,
    PullRequestComment,
    PullRequestReview,
}

impl Subject {
    pub fn name(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::IssueComment => "issue_comment",
            Self::PullRequest => "pull_request",
            Self::PullRequestComment => "pr_comment",
            Self::PullRequestReview => "pr_review",
        }
    }
}

/// The reaction has no target: the item is not in the repository, or the comment or review is not on
/// the item or is still pending.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum ReactionRefusal {
    #[error("reaction_target_not_found")]
    NotFound,
}

/// One reaction on a subject: how many users gave it, and whether the viewer is one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionCount {
    pub subject_id: i64,
    pub content: String,
    pub count: i64,
    pub mine: bool,
}

// Adds the reaction, or removes it when the user already gave it. Returns whether it is now set.
// The reactor's access, the item and the subject are checked in the same transaction as the change.
native_operation!(crate::backend::db::postgres::reactions::toggle_reaction,
    crate::backend::db::sqlite::reactions::toggle_reaction,
    fn toggle_reaction(pool: &DbPool, actor_id: i64, repo_id: i64, item_id: i64, subject: Subject, subject_id: i64, content: &str) -> Result<bool>);

native_operation!(crate::backend::db::postgres::reactions::reaction_counts,
    crate::backend::db::sqlite::reactions::reaction_counts,
    fn reaction_counts(pool: &DbPool, subject: &str, ids: &[i64], viewer_id: i64) -> Result<Vec<ReactionCount>>);

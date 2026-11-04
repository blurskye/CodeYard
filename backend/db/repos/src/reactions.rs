use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

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
    Release,
    CommitComment,
}

impl Subject {
    pub fn name(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::IssueComment => "issue_comment",
            Self::PullRequest => "pull_request",
            Self::PullRequestComment => "pr_comment",
            Self::PullRequestReview => "pr_review",
            Self::Release => "release",
            Self::CommitComment => "commit_comment",
        }
    }
}

pub fn valid_content(subject: Subject, content: &str) -> bool {
    CONTENTS.contains(&content)
        && (subject != Subject::Release || !matches!(content, "-1" | "confused"))
}

/// The reaction has no target: the item is not in the repository, or the comment or review is not on
/// the item or is still pending.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum ReactionRefusal {
    #[error("reaction_target_not_found")]
    NotFound,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Reaction {
    pub id: i64,
    pub repo_id: i64,
    pub user_id: i64,
    pub subject_kind: String,
    pub subject_id: i64,
    pub content: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// The item and subject are checked together before a reaction is changed.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub repo_id: i64,
    pub item_id: i64,
    pub subject: Subject,
    pub subject_id: i64,
}

native_operation!(crate::postgres::reactions::create_reaction,
    crate::sqlite::reactions::create_reaction,
    fn create_reaction(pool: &DbPool, actor_id: i64, credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>, target: Target, content: &str) -> Result<(Reaction, bool)>);

native_operation!(crate::postgres::reactions::delete_reaction,
    crate::sqlite::reactions::delete_reaction,
    fn delete_reaction(pool: &DbPool, actor_id: i64, credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>, target: Target, reaction_id: i64) -> Result<bool>);

native_operation!(crate::postgres::reactions::find_reaction,
    crate::sqlite::reactions::find_reaction,
    fn find_reaction(pool:&DbPool,actor:i64,target:Target,content:&str)->Result<Option<Reaction>>);

native_operation!(crate::postgres::reactions::list_reactions,
    crate::sqlite::reactions::list_reactions,
    fn list_reactions(pool: &DbPool, target: Target, content: Option<&str>, limit: i64, offset: i64) -> Result<Vec<Reaction>>);

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
native_operation!(crate::postgres::reactions::toggle_reaction,
    crate::sqlite::reactions::toggle_reaction,
    fn toggle_reaction(pool: &DbPool, actor_id: i64, credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>, repo_id: i64, item_id: i64, subject: Subject, subject_id: i64, content: &str) -> Result<bool>);

native_operation!(crate::postgres::reactions::reaction_counts,
    crate::sqlite::reactions::reaction_counts,
    fn reaction_counts(pool: &DbPool, subject: &str, ids: &[i64], viewer_id: i64) -> Result<Vec<ReactionCount>>);

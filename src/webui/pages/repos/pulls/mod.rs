//! Pull request pages: the commits, checks and files-changed views, and the review panels shared
//! with the discussion.
pub(super) mod checks;
pub(super) mod commits;
pub(super) mod diff;
pub(super) mod review;

pub use checks::checks_page;
pub use commits::{commits_page, MAX_LISTED_COMMITS};
pub use diff::{diff_page, Diff, DiffFile, Viewed};
pub use review::{
    Conversation, LineComment, Review, ReviewDraft, Reviewer, ReviewerStatus, Thread,
};

/// A pull request that a branch or a comparison links to: its number and title.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PullSummary {
    pub number: i32,
    pub title: String,
}

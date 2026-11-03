//! Commit history and commit detail: `/:owner/:repo/commits` and `/:owner/:repo/commit/:sha`.
pub mod comments;
mod commit_target;
mod page;
pub mod pulls;

pub(in crate::pages::repos) use commit_target::commit_target;
pub(in crate::pages::repos) use page::{commit_date, commit_row, history_url, latest_commit_bar};
pub use page::{commit_detail, commits, Commit, HistoryFilter};

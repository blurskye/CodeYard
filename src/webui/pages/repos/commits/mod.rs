//! Commit history and commit detail: `/:owner/:repo/commits` and `/:owner/:repo/commit/:sha`.
mod commit_target;
mod page;

pub(in crate::webui::pages::repos) use commit_target::commit_target;
pub(in crate::webui::pages::repos) use page::{commit_date, commit_row, latest_commit_bar};
pub use page::{commit_detail, commits, Commit, HistoryFilter};

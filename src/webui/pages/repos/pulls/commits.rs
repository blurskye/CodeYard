//! The Commits tab of a pull request: the commits its head has that its base does not.
use crate::webui::pages::repos::{
    self as repos,
    commits::{commit_row, Commit},
    issues::{
        detail::{pull_summary, pull_tabs, PullTab},
        Item, Kind, Repository,
    },
};
use dioxus::prelude::*;

/// The most commits the Commits tab lists. A pull request with more shows these, and says so.
pub const MAX_LISTED_COMMITS: usize = 100;

/// The Commits tab: the commits between the base and the head of a pull request, newest first.
pub fn commits_page(repo: &Repository, item: &Item, commits: &[Commit]) -> Element {
    let url = format!("{}/{}", repo.family_url(Kind::Pull), item.number);
    let base = repo.url();
    repos::chrome::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-pull-commits",
                header { class: "collaboration-detail-heading",
                    h2 { "{item.title} ", span { class: "collaboration-number", "#{item.number}" } }
                    {pull_summary(repo, item, Kind::Pull)}
                    {pull_tabs(&url, item, PullTab::Commits)}
                }
                if commits.is_empty() {
                    p { class: "collaboration-panel collaboration-muted", "No commits sit between the base and the head." }
                } else {
                    div { class: "repo-commit-list panel", aria_label: "Commits",
                        for commit in commits {
                            {commit_row(&base, commit)}
                        }
                    }
                }
                if commits.len() >= MAX_LISTED_COMMITS {
                    p { class: "collaboration-muted", "Only the newest {MAX_LISTED_COMMITS} commits are listed." }
                }
            }
        },
    )
}

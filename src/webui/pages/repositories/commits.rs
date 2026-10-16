use super::{
    chrome::{chrome, icon},
    ref_path::file_url,
    RepoChrome,
};
use crate::webui::{
    components::{compact_date, count_label, patch_badges, patch_lines, Avatar, DraftInput},
    navigation::NavLink,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Commit {
    pub sha: String,
    pub message: String,
    pub author: String,
    pub author_date: String,
    pub committer: String,
    pub committer_date: String,
    pub parents: Vec<String>,
}

pub(super) fn commit_date(value: &str) -> Element {
    rsx! { time { datetime: value.to_owned(), title: value.to_owned(), "{compact_date(value)}" } }
}

pub(super) fn latest_commit_bar(base: &str, reference: &str, commit: &Commit) -> Element {
    let url = format!(
        "{base}/commit/{}",
        crate::webui::encode_component(&commit.sha)
    );
    rsx! {
        div { class: "repo-latest-commit bg-muted/40 border-b border-border flex items-center justify-between gap-4 text-sm",
            div { class: "repo-commit-summary min-w-0",
                {icon("commits")}
                span { class: "repo-commit-author", Avatar { name: commit.author.clone(), size: 20 } "{commit.author}" }
                NavLink { class: "repo-commit-message".to_owned(), href: url.clone(),
                    span { title: commit.message.clone(), "{commit.message.lines().next().unwrap_or(\"\")}" }
                }
            }
            div { class: "repo-commit-meta",
                NavLink { class: "repo-short-sha".to_owned(), href: url, "{commit.sha.get(..7).unwrap_or(&commit.sha)}" }
                {commit_date(&commit.author_date)}
                NavLink { class: "repo-history-link".to_owned(), href: format!("{base}/commits?ref={}", crate::webui::encode_component(reference)), {icon("commits")} span { "History" } }
            }
        }
    }
}

fn commit_row(base: &str, commit: &Commit) -> Element {
    let href = format!(
        "{base}/commit/{}",
        crate::webui::encode_component(&commit.sha)
    );
    rsx! {
        article { class: "repo-commit-row",
            div { class: "repo-commit-node", {icon("commits")} }
            div { class: "repo-commit-row-content",
                NavLink { class: "repo-history-message".to_owned(), href: href.clone(), "{commit.message.lines().next().unwrap_or(\"\")}" }
                p { class: "repo-history-author", Avatar { name: commit.author.clone(), size: 20 } strong { "{commit.author}" } " committed " {commit_date(&commit.author_date)} }
            }
            NavLink { class: "repo-short-sha".to_owned(), href, "{commit.sha.get(..7).unwrap_or(&commit.sha)}" }
        }
    }
}

pub fn commits(
    repo: &RepoChrome,
    rows: &[Commit],
    reference: &str,
    page: usize,
    has_more: bool,
) -> Element {
    let base = repo.base();
    chrome(
        repo,
        rsx! {
            section { class: "repo-history-page",
                header { class: "repo-page-heading",
                    div {
                        h2 { "Commits" }
                        p { "History for " code { class: "repo-ref-pill", "{reference}" } }
                    }
                    CommitFilter { base: base.clone(), reference: reference.to_owned() }
                }
                if rows.is_empty() {
                    div { class: "repo-empty panel", {icon("commits")} h3 { "No commits on this ref" } p { "Choose another branch, tag, or commit to view its history." } }
                } else {
                    div { class: "repo-commit-list panel", for commit in rows { {commit_row(&base, commit)} } }
                }
                nav { class: "repo-pagination", "aria-label": "Commit history",
                    if page > 1 { NavLink { class: "btn btn-secondary".to_owned(), href: format!("{base}/commits?ref={}&page={}", crate::webui::encode_component(reference), page - 1), "Previous" } }
                    span { "Page {page}" }
                    if has_more { NavLink { class: "btn btn-secondary".to_owned(), href: format!("{base}/commits?ref={}&page={}", crate::webui::encode_component(reference), page + 1), "Next" } }
                }
            }
        },
    )
}

#[component]
fn CommitFilter(base: String, reference: String) -> Element {
    rsx! {
        form { method: "get", action: format!("{base}/commits"), class: "repo-compact-form",
            label { class: "repo-sr-only", r#for: "commit-ref", "Branch, tag, or commit" }
            DraftInput { id: "commit-ref", name: "ref", initial_value: reference }
            button { class: "btn btn-secondary", r#type: "submit", "Go" }
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiffFile {
    pub path: String,
    pub status: String,
    pub additions: i32,
    pub deletions: i32,
    pub patch: String,
}

pub fn commit_detail(repo: &RepoChrome, commit: &Commit, files: &[DiffFile]) -> Element {
    let base = repo.base();
    chrome(
        repo,
        rsx! {
            article { class: "repo-commit-detail panel",
                header { class: "repo-commit-detail-header",
                    h2 { "{commit.message.lines().next().unwrap_or(\"Commit\")}" }
                    if commit.message.lines().count() > 1 { pre { "{commit.message}" } }
                }
                div { class: "repo-commit-detail-meta",
                    div { {icon("commits")} p { strong { "{commit.author}" } " authored and " strong { "{commit.committer}" } " committed " {commit_date(&commit.committer_date)} } }
                    code { class: "repo-full-sha", "{commit.sha}" }
                }
                footer { class: "repo-commit-detail-footer",
                    if !commit.parents.is_empty() {
                        span { "Parents " for parent in &commit.parents { NavLink { class: "repo-short-sha".to_owned(), href: format!("{base}/commit/{}", crate::webui::encode_component(parent)), "{parent.get(..7).unwrap_or(parent)}" } } }
                    }
                    NavLink { class: "btn btn-secondary".to_owned(), href: file_url(&base, "tree", &commit.sha, ""), "Browse files" }
                }
            }
            section { class: "repo-diff-section", aria_label: "Changes in this commit",
                header { class: "repo-section-heading",
                    h3 { "Files changed" }
                    span { {count_label(files.len(), "file", "files")} }
                }
                if files.is_empty() { div { class: "repo-empty panel", p { "No file changes in this commit." } } }
                for file in files {
                    details { class: "repo-diff-file panel", open: true,
                        summary { class: "repo-diff-header",
                            span { class: "repo-diff-path", "{file.path}" }
                            for badge in patch_badges(&file.status, &file.patch) { span { class: "patch-badge", "{badge}" } }
                            span { class: "repo-diff-additions", "+{file.additions}" }
                            span { class: "repo-diff-deletions", "−{file.deletions}" }
                        }
                        pre { class: "repo-diff-patch", {patch_lines(&file.patch)} }
                    }
                }
            }
        },
    )
}

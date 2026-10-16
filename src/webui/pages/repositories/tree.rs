use super::{
    chrome::{chrome, icon},
    commits::{commit_date, latest_commit_bar},
    ref_path::{file_url, link_base},
    Commit, RepoChrome,
};
use crate::webui::{
    components::{count_label, references::References, CopyButton},
    navigation::NavLink,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub size: Option<u64>,
    pub last_commit: Option<Commit>,
}
/// Maximum repository README size rendered inline; larger files keep their file view.
pub const MAX_README_BYTES: u64 = 500 * 1024;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TreePage {
    pub reference: String,
    pub path: String,
    pub refs: Vec<String>,
    /// Branch and tag counts, from the refs the page already read.
    pub branch_count: usize,
    pub tag_count: usize,
    pub entries: Vec<TreeEntry>,
    pub readme: Option<String>,
    pub latest_commit: Option<Commit>,
    pub empty: bool,
    pub http_clone: String,
    pub ssh_clone: String,
    /// The references the README links; empty when there is no README. Boxed, because the
    /// repository page model's largest variant holds this struct.
    #[serde(default)]
    pub references: Box<References>,
}

pub fn tree(repo: &RepoChrome, page: &TreePage) -> Element {
    let base = repo.base();
    let branches = count_label(page.branch_count, "branch", "branches");
    let tags = count_label(page.tag_count, "tag", "tags");
    let parent = page
        .path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    let oversized_readme = page
        .entries
        .iter()
        .find(|entry| entry.kind == "file" && entry.name.eq_ignore_ascii_case("readme.md"))
        .filter(|entry| entry.size.is_some_and(|size| size > MAX_README_BYTES));
    let clone_id = crate::webui::encode_component(&base);
    let https_id = format!("clone-https-{clone_id}");
    let ssh_id = format!("clone-ssh-{clone_id}");
    let mut entries: Vec<_> = page.entries.iter().collect();
    entries.sort_by(|a, b| {
        (a.kind != "tree")
            .cmp(&(b.kind != "tree"))
            .then_with(|| a.name.cmp(&b.name))
    });
    chrome(
        repo,
        rsx! {
            div { class: "repo-tree-layout",
                section { class: "repo-tree-main min-w-0 space-y-4",
                    div { class: "repo-tree-toolbar flex flex-wrap items-center justify-between gap-2",
                        div { class: "repo-ref-controls flex flex-wrap items-center gap-3",
                            details { class: "repo-menu repo-ref-menu",
                                summary { class: "btn btn-secondary", {icon("branches")} span { class: "repo-ref-name", "{page.reference}" } {icon("chevron")} }
                                div { class: "repo-menu-panel repo-ref-panel",
                                    h2 { class: "repo-menu-heading", "Switch branch/tag" }
                                    ul { class: "repo-ref-list",
                                        for reference in &page.refs {
                                            li { key: "{reference}", NavLink { class: (if reference == &page.reference { "repo-menu-item repo-ref-selected" } else { "repo-menu-item" }).to_owned(), href: file_url(&base, "tree", reference, &page.path), "{reference}" } }
                                        }
                                    }
                                    div { class: "repo-menu-footer", NavLink { href: format!("{base}/branches"), "View all branches" } NavLink { href: format!("{base}/tags"), "View all tags" } }
                                }
                            }
                            if !page.empty { NavLink { class: "btn btn-secondary".to_owned(), href: file_url(&base, "find", &page.reference, ""), {icon("search")} "Go to file" } }
                            NavLink { class: "repo-ref-link".to_owned(), href: format!("{base}/branches"), {icon("branches")} "{branches}" }
                            NavLink { class: "repo-ref-link".to_owned(), href: format!("{base}/tags"), {icon("tags")} "{tags}" }
                        }
                        div { class: "repo-file-actions flex items-center gap-2",
                            if repo.can_write {
                                details { class: "repo-menu repo-add-menu",
                                    summary { class: "btn btn-secondary", {icon("plus")} "Add file" {icon("chevron")} }
                                    div { class: "repo-menu-panel", NavLink { class: "repo-menu-item".to_owned(), href: file_url(&base, "new", &page.reference, &page.path), "Create new file" } NavLink { class: "repo-menu-item".to_owned(), href: file_url(&base, "upload", &page.reference, &page.path), "Upload files" } }
                                }
                            }
                            details { class: "repo-menu repo-code-menu",
                                summary { class: "btn repo-code-button", {icon("download")} "Code" {icon("chevron")} }
                                div { class: "repo-menu-panel repo-clone-panel",
                                    h2 { class: "repo-menu-heading", {icon("code")} "Clone" }
                                    div { class: "repo-clone-protocols",
                                        // Native radio tabs retain no-JS URL selection. The static, nonvolatile
                                        // default does not reset a user's protocol choice on parent rerenders.
                                        input { r#type: "radio", name: format!("clone-protocol-{clone_id}"), id: https_id.clone(), class: "repo-sr-only repo-protocol-radio repo-protocol-https", "checked": true }
                                        if !page.ssh_clone.is_empty() {
                                            input { r#type: "radio", name: format!("clone-protocol-{clone_id}"), id: ssh_id.clone(), class: "repo-sr-only repo-protocol-radio repo-protocol-ssh" }
                                        }
                                        div { class: "repo-clone-tabs",
                                            label { "for": https_id, "data-protocol": "https", "HTTPS" }
                                            if !page.ssh_clone.is_empty() { label { "for": ssh_id, "data-protocol": "ssh", "SSH" } }
                                        }
                                        div { class: "repo-clone-https-field", {clone_url("HTTPS", &page.http_clone)} }
                                        if !page.ssh_clone.is_empty() { div { class: "repo-clone-ssh-field", {clone_url("SSH", &page.ssh_clone)} } }
                                    }
                                    p { class: "repo-clone-help", "Select a URL to copy it." }
                                    if !page.empty {
                                        a { class: "repo-menu-item repo-download", href: format!("/api/repos{base}/archive?ref={}", crate::webui::encode_component(&page.reference)), {icon("download")} "Download ZIP" }
                                    }
                                }
                            }
                        }
                    }
                    if !page.path.is_empty() { {breadcrumbs(repo, &page.reference, &page.path, false)} }
                    if page.empty {
                        section { class: "repo-empty panel",
                            h2 { "This repository is empty" }
                            p { "Push an existing repository or add your first file." }
                            if repo.can_write {
                                NavLink { class: "btn btn-secondary".to_owned(), href: file_url(&base, "new", &page.reference, &page.path), "Create new file" }
                                NavLink { class: "btn btn-secondary".to_owned(), href: file_url(&base, "upload", &page.reference, &page.path), "Upload files" }
                                {quick_setup(&page.http_clone, &page.reference)}
                            }
                        }
                    } else {
                        div { class: "repo-file-browser border border-border rounded-md bg-card",
                            if let Some(commit) = &page.latest_commit { {latest_commit_bar(&base, &page.reference, commit)} }
                            table { class: "repo-file-table w-full border-collapse text-sm",
                                caption { class: "repo-sr-only", if page.path.is_empty() { "Repository files" } else { "Files in {page.path}" } }
                                thead { class: "repo-sr-only", tr { th { scope: "col", "Name" } th { scope: "col", "Last commit" } th { scope: "col", "Last updated" } } }
                                tbody {
                                    if !page.path.is_empty() { tr { class: "repo-parent-row", td { colspan: "3", NavLink { href: file_url(&base, "tree", &page.reference, parent), ".." } } } }
                                    for entry in entries {
                                        tr { key: "{entry.path}",
                                            td { class: "repo-file-name",
                                                div { class: "repo-file-name-inner",
                                                    {icon(if entry.kind == "tree" { "folder" } else { "file" })}
                                                    NavLink { href: file_url(&base, if entry.kind == "tree" { "tree" } else { "blob" }, &page.reference, &entry.path), "{entry.name}" }
                                                }
                                            }
                                            td { class: "repo-file-message",
                                                if let Some(commit) = &entry.last_commit {
                                                    NavLink { href: format!("{base}/commit/{}", crate::webui::encode_component(&commit.sha)),
                                                        span { title: commit.message.clone(), "{commit.message.lines().next().unwrap_or(\"\")}" }
                                                    }
                                                }
                                            }
                                            td { class: "repo-file-date",
                                                if let Some(commit) = &entry.last_commit { {commit_date(&commit.author_date)} }
                                                else if let Some(size) = entry.size { span { "{size} B" } }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Some(readme) = &page.readme {
                        section { id: "readme", class: "repo-readme border border-border rounded-md bg-card",
                            h2 { class: "repo-readme-header", {icon("readme")} "README" }
                            div { class: "repo-readme-body", crate::webui::components::RepoMarkdown { content: readme.clone(), base: link_base(&base, &page.reference, &page.path), references: (*page.references).clone() } }
                        }
                    } else if let Some(entry) = oversized_readme {
                        section { id: "readme", class: "repo-readme border border-border rounded-md bg-card",
                            h2 { class: "repo-readme-header", {icon("readme")} "README" }
                            div { class: "repo-readme-body",
                                p { "This README is larger than 500 KiB and is not rendered inline. "
                                    NavLink { href: file_url(&base, "blob", &page.reference, &entry.path), "View file" }
                                }
                            }
                        }
                    }
                }
                {about(repo, page.readme.is_some() || oversized_readme.is_some())}
            }
        },
    )
}

/// GitHub-style setup commands for an empty repository, as plain preformatted text.
fn quick_setup(url: &str, branch: &str) -> Element {
    let new_repository = format!(
        "git init\ngit add README.md\ngit commit -m \"first commit\"\ngit branch -M {branch}\ngit remote add origin {url}\ngit push -u origin {branch}"
    );
    let existing_repository =
        format!("git remote add origin {url}\ngit branch -M {branch}\ngit push -u origin {branch}");
    rsx! {
        div { class: "repo-quick-setup",
            h3 { "Quick setup" }
            {clone_url("HTTPS", url)}
            h4 { "Create a new repository on the command line" }
            pre { class: "repo-quick-setup-code", "{new_repository}" }
            h4 { "Push an existing repository from the command line" }
            pre { class: "repo-quick-setup-code", "{existing_repository}" }
        }
    }
}

fn clone_url(protocol: &str, url: &str) -> Element {
    rsx! {
        div { class: "repo-clone-protocol",
            label { class: "repo-clone-label", span { class: "repo-sr-only", "{protocol} clone URL" }
                input { readonly: true, "value": url.to_owned(), "aria-label": format!("{protocol} clone URL"), class: "repo-clone-url" }
            }
            div { class: "repo-clone-copy", CopyButton { text: url.to_owned(), label: format!("Copy {protocol}") } }
        }
    }
}

fn about(repo: &RepoChrome, has_readme: bool) -> Element {
    rsx! {
        aside { class: "repo-about", "aria-label": "About this repository",
            section { class: "repo-about-section",
                div { class: "repo-about-heading",
                    h2 { "About" }
                    if repo.can_admin { NavLink { class: "repo-about-settings".to_owned(), href: format!("{}/settings", repo.base()), {icon("settings")} span { class: "repo-sr-only", "Repository settings" } } }
                }
                if !repo.description.is_empty() { p { class: "repo-description", "{repo.description}" } }
                ul { class: "repo-about-stats",
                    if has_readme { li { a { href: "#readme", {icon("readme")} "Readme" } } }
                    li { NavLink { href: format!("{}/commits", repo.base()), {icon("commits")} "Activity" } }
                    li { {icon("star")} strong { "{repo.stars}" } " stars" }
                    li { {icon("watch")} strong { "{repo.watchers}" } " watching" }
                    li { {icon("fork")} strong { "{repo.forks}" } " forks" }
                }
            }
        }
    }
}

pub(super) fn breadcrumbs(repo: &RepoChrome, reference: &str, path: &str, file: bool) -> Element {
    let base = repo.base();
    let parts: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    rsx! {
        nav { class: "repo-breadcrumbs", "aria-label": "File path",
            NavLink { href: file_url(&base, "tree", reference, ""), "{repo.name}" }
            for (index, part) in parts.iter().enumerate() {
                span { key: "{index}", class: "repo-breadcrumb-part",
                    span { class: "repo-path-separator", "/" }
                    if file && index + 1 == parts.len() { span { class: "repo-path-current", "{part}" } }
                    else { NavLink { href: file_url(&base, "tree", reference, &parts[..=index].join("/")), "{part}" } }
                }
            }
        }
    }
}

use crate::webui::pages::discovery::topic_url;
use crate::webui::pages::repos::{
    blob::{
        add_file_menu, more_menu, outline_menu, title_row, FilesPanel, FolderListing, GoToFile,
    },
    chrome::{chrome, csrf, icon, repository_id, RepoChrome},
    commits::{commit_date, latest_commit_bar, Commit},
    ref_path::{file_url, link_base},
    ref_switcher::{ref_options, RefSwitcher},
};
use crate::webui::{
    components::{
        count_label, document_html,
        languages::{language_bar, LanguageShare},
        outline_of_html,
        references::References,
        CopyButton, DraftInput, DraftTextarea,
    },
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
    /// Every branch and every tag, sorted, for the ref menu and the counts.
    pub branches: Vec<String>,
    pub tags: Vec<String>,
    pub entries: Vec<TreeEntry>,
    /// The root and each folder on the way to this one, for the Files panel. This folder's own
    /// listing comes last.
    #[serde(default)]
    pub folders: Vec<FolderListing>,
    /// The default branch's languages, for the About panel. Empty when there are none.
    #[serde(default)]
    pub languages: Vec<LanguageShare>,
    /// The release count and latest tag, for the About panel. Only the repository root has it.
    #[serde(default)]
    pub releases: Option<crate::webui::pages::repos::releases::ReleasesSummary>,
    pub readme: Option<String>,
    pub latest_commit: Option<Commit>,
    /// This folder at the head commit, so the link keeps showing it. Empty when there is none.
    #[serde(default)]
    pub permalink: String,
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
    let home = page.path.is_empty();
    let oversized_readme = page
        .entries
        .iter()
        .find(|entry| entry.kind == "file" && entry.name.eq_ignore_ascii_case("readme.md"))
        .filter(|entry| entry.size.is_some_and(|size| size > MAX_README_BYTES));
    let readme_path = page
        .entries
        .iter()
        .find(|entry| entry.kind == "file" && entry.name.eq_ignore_ascii_case("readme.md"))
        .map(|entry| entry.path.clone());
    let readme_html = page.readme.as_deref().map(|readme| {
        document_html(
            readme,
            &page.references,
            &link_base(&base, &page.reference, &page.path),
        )
    });
    let outline = readme_html
        .as_deref()
        .map(outline_of_html)
        .unwrap_or_default();
    let mut entries: Vec<_> = page.entries.iter().collect();
    entries.sort_by(|a, b| {
        (a.kind != "tree")
            .cmp(&(b.kind != "tree"))
            .then_with(|| a.name.cmp(&b.name))
    });
    // The repository home has its own toolbar and no Files panel. A folder keeps the title row.
    let top = if home {
        home_toolbar(repo, page)
    } else {
        let actions = rsx! {
            {ref_counts(repo, page)}
            {goto_file_icon(repo, page)}
            {add_file(repo, page)}
            {code_menu(repo, page)}
            {more_menu(&page.path, &page.permalink, None, None)}
        };
        title_row(repo, &page.reference, &page.path, !page.empty, actions)
    };
    let main = rsx! {
        div { class: "repo-blob-main",
            {top}
            if page.empty {
                section { class: "repo-empty panel",
                    h2 { "This repository is empty" }
                    p { "Push an existing repository or add your first file." }
                    if repo.can_write {
                        div { class: "repo-empty-actions",
                            NavLink { class: "btn btn-primary".to_owned(), href: file_url(&base, "new", &page.reference, &page.path), "Create new file" }
                            NavLink { class: "btn btn-secondary".to_owned(), href: file_url(&base, "upload", &page.reference, &page.path), "Upload files" }
                        }
                        {quick_setup(&page.http_clone, &page.ssh_clone, &page.reference)}
                    }
                }
            } else {
                div { class: "repo-file-browser border border-border rounded-md bg-card",
                    if let Some(commit) = &page.latest_commit { {latest_commit_bar(&base, &page.reference, &page.path, commit)} }
                    table { class: "repo-file-table w-full border-collapse text-sm",
                        caption { class: "repo-sr-only", if page.path.is_empty() { "Repository files" } else { "Files in {page.path}" } }
                        thead { class: "repo-sr-only", tr { th { scope: "col", "Name" } th { scope: "col", "Last commit" } th { scope: "col", "Last updated" } } }
                        tbody {
                            if !page.path.is_empty() { tr { class: "repo-parent-row", td { colspan: "3", NavLink { href: file_url(&base, "tree", &page.reference, parent_of(&page.path)), ".." } } } }
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
            if let Some(html) = &readme_html {
                section { id: "readme", class: "repo-readme border border-border rounded-md bg-card",
                    div { class: "repo-readme-header",
                        h2 { class: "repo-readme-title", {icon("readme")} "README" }
                        div { class: "repo-readme-actions",
                            if outline.len() > 1 { {outline_menu(&outline)} }
                            if repo.can_write {
                                if let Some(path) = &readme_path {
                                    NavLink { class: "repo-readme-edit icon-tooltip".to_owned(), href: file_url(&base, "edit", &page.reference, path), aria_label: Some("Edit README".to_owned()), {icon("edit")} }
                                }
                            }
                        }
                    }
                    div { class: "repo-readme-body", div { class: "markdown-body leading-relaxed text-foreground", dangerous_inner_html: html.clone() } }
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
    };
    let layout = if home {
        rsx! {
            div { class: "repo-home-layout",
                {main}
                {about_column(repo, page)}
            }
        }
    } else {
        let branch_options = ref_options(&base, "tree", &page.path, &page.branches, "");
        let tag_options = ref_options(&base, "tree", &page.path, &page.tags, "refs/tags/");
        rsx! {
            div { class: "repo-blob-layout",
                if !page.empty {
                    input { r#type: "checkbox", id: "repo-files-toggle", class: "repo-sr-only repo-files-toggle" }
                    FilesPanel { base: base.clone(), reference: page.reference.clone(), path: page.path.clone(), directory: page.path.clone(), folders: page.folders.clone(), branches: branch_options, tags: tag_options, can_write: repo.can_write, default_branch: repo.default_branch.clone() }
                }
                {main}
            }
        }
    };
    chrome(repo, layout)
}

/// The branch and tag counts, each a link to its list.
fn ref_counts(repo: &RepoChrome, page: &TreePage) -> Element {
    let base = repo.base();
    let branches = count_label(page.branches.len(), "branch", "branches");
    let tags = count_label(page.tags.len(), "tag", "tags");
    rsx! {
        NavLink { class: "repo-ref-link".to_owned(), href: format!("{base}/branches"), {icon("branches")} "{branches}" }
        NavLink { class: "repo-ref-link".to_owned(), href: format!("{base}/tags"), {icon("tags")} "{tags}" }
    }
}

/// Add file for this folder or the root. An empty repository offers both choices in its body instead.
fn add_file(repo: &RepoChrome, page: &TreePage) -> Element {
    if !repo.can_write || page.empty {
        return rsx! {};
    }
    add_file_menu(
        &repo.base(),
        &page.reference,
        &page.path,
        rsx! { summary { class: "btn btn-secondary", "aria-label": "Add file", {icon("plus")} span { class: "repo-add-label", "Add file" } {icon("chevron")} } },
    )
}

/// Go to file as an icon, for a phone: the box that finds files is hidden there, and this link opens the
/// finder page, which lists every file at the revision. An empty repository has no files to find.
fn goto_file_icon(repo: &RepoChrome, page: &TreePage) -> Element {
    if page.empty {
        return rsx! {};
    }
    rsx! {
        NavLink { class: "repo-icon-button repo-goto-phone icon-tooltip".to_owned(), href: file_url(&repo.base(), "find", &page.reference, ""), aria_label: Some("Go to file".to_owned()),
            {icon("search")}
        }
    }
}

/// The Code menu: the clone URLs and the ZIP download.
fn code_menu(repo: &RepoChrome, page: &TreePage) -> Element {
    let base = repo.base();
    // A short name shows the branch of that name when there is one, so a tag with no branch of its
    // own is downloaded by its full ref.
    let archive_ref =
        if page.tags.contains(&page.reference) && !page.branches.contains(&page.reference) {
            format!("refs/tags/{}", page.reference)
        } else {
            page.reference.clone()
        };
    let archive_ref = crate::webui::encode_component(&archive_ref);
    rsx! {
        details { class: "native-menu repo-menu repo-code-menu",
            summary { class: "btn repo-code-button", "aria-label": "Code", {icon("code")} span { class: "repo-button-label", "Code" } {icon("chevron")} }
            div { class: "repo-menu-panel repo-clone-panel",
                h2 { class: "repo-menu-heading", {icon("code")} "Clone" }
                div { class: "repo-clone-urls",
                    {clone_choice("clone-menu", &page.http_clone, &page.ssh_clone)}
                }
                p { class: "repo-clone-help", "Copy a URL to clone this repository." }
                if !page.empty {
                    a { class: "repo-menu-item repo-download", href: format!("{base}/archive/{archive_ref}.zip"), {icon("download")} "Download ZIP" }
                    a { class: "repo-menu-item repo-download", href: format!("{base}/archive/{archive_ref}.tar.gz"), {icon("download")} "Download tar.gz" }
                }
            }
        }
    }
}

/// The toolbar above the repository home's file list. On the left: the branch menu, the branch and
/// tag counts, and Go to file. On the right: Add file and Code. An empty repository has no branch
/// menu or search, only the counts and Code.
fn home_toolbar(repo: &RepoChrome, page: &TreePage) -> Element {
    let base = repo.base();
    let branch_options = ref_options(&base, "tree", &page.path, &page.branches, "");
    let tag_options = ref_options(&base, "tree", &page.path, &page.tags, "refs/tags/");
    rsx! {
        div { class: "repo-home-toolbar",
            div { class: "repo-home-refs",
                if !page.empty {
                    RefSwitcher { base: base.clone(), current: page.reference.clone(), branches: branch_options, tags: tag_options, default_branch: repo.default_branch.clone() }
                }
                {ref_counts(repo, page)}
                if !page.empty {
                    GoToFile { base: base.clone(), reference: page.reference.clone(), folders: page.folders.clone() }
                }
            }
            div { class: "repo-home-actions",
                {goto_file_icon(repo, page)}
                {add_file(repo, page)}
                {code_menu(repo, page)}
            }
        }
    }
}

/// The About column of the repository home: the description, the website, the topics, the counts
/// and the links that are known. Writers edit the first three with the gear. Folders and files
/// have none.
fn about_column(repo: &RepoChrome, page: &TreePage) -> Element {
    let base = repo.base();
    let stars = count_label(repo.stars.max(0) as usize, "star", "stars");
    let watchers = count_label(repo.watchers.max(0) as usize, "watcher", "watchers");
    let forks = count_label(repo.forks.max(0) as usize, "fork", "forks");
    let license = page.entries.iter().find(|entry| {
        entry.kind == "file" && entry.name.to_ascii_lowercase().starts_with("license")
    });
    let website_is_link =
        repo.website.starts_with("https://") || repo.website.starts_with("http://");
    rsx! {
        aside { class: "repo-about", "aria-label": "About",
            div { class: "repo-about-header",
                h2 { class: "repo-about-heading", "About" }
                if repo.can_write { {about_edit(repo)} }
            }
            if !repo.description.is_empty() { p { class: "repo-description", "{repo.description}" } }
            if website_is_link {
                a { class: "repo-about-website", href: repo.website.clone(), title: repo.website.clone(), rel: "nofollow noopener noreferrer",
                    {icon("link")} span { "{website_host(&repo.website)}" }
                }
            }
            if !repo.topics.is_empty() {
                ul { class: "repo-topics", aria_label: "Topics",
                    for topic in &repo.topics { li { NavLink { href: topic_url(topic), class: "repo-topic", "{topic}" } } }
                }
            }
            ul { class: "repo-about-list",
                li { {icon("star")} span { "{stars}" } }
                li { {icon("watch")} span { "{watchers}" } }
                li { {icon("fork")} span { "{forks}" } }
                if page.readme.is_some() { li { {icon("readme")} a { href: "#readme", "Readme" } } }
                if let Some(entry) = license { li { {icon("file")} NavLink { href: file_url(&base, "blob", &page.reference, &entry.path), "License" } } }
            }
            if let Some(summary) = &page.releases { {crate::webui::pages::repos::releases::sidebar(repo, summary)} }
            {language_bar(&page.languages)}
        }
    }
}

/// The host of a website, such as `example.com` for `https://example.com/docs`. The link keeps
/// the whole address as its target and tooltip.
fn website_host(website: &str) -> &str {
    let rest = website.split_once("://").map_or(website, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() {
        website
    } else {
        host
    }
}

/// The gear that opens the About form. It is a native details element that posts to the about
/// action, so it works without scripts.
fn about_edit(repo: &RepoChrome) -> Element {
    let topics = repo.topics.join(", ");
    rsx! {
        details { class: "repo-about-edit",
            summary { class: "repo-about-edit-button icon-tooltip", "aria-label": "Edit repository details", {icon("settings")} }
            form { class: "repo-about-form", method: "post", action: repo.action("about"),
                {csrf(&repo.csrf_token)}
                {repository_id(repo.id)}
                label { class: "repo-field", span { "Description" }
                    DraftTextarea { name: "description", rows: 3, maxlength: Some(2000), initial_value: repo.description.clone() }
                }
                label { class: "repo-field", span { "Website" }
                    DraftInput { name: "website", input_type: "url", maxlength: Some(255), placeholder: "https://example.com", initial_value: repo.website.clone() }
                }
                label { class: "repo-field", span { "Topics" }
                    DraftInput { name: "topics", placeholder: "rust, git, forge", initial_value: topics }
                    small { "Separate topics with commas or spaces. Use lowercase letters, numbers and hyphens." }
                }
                div { class: "repo-form-actions", button { class: "btn btn-primary", r#type: "submit", "Save details" } }
            }
        }
    }
}

/// The folder above `path`, or the root for a folder at the top.
fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn shell_argument(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Setup for an empty repository: the URL with an HTTPS or SSH choice, a line on what to add
/// first, then the commands as plain preformatted text. The first command makes the commit from
/// whatever the folder holds, since an empty repository has no README yet.
fn quick_setup(http: &str, ssh: &str, branch: &str) -> Element {
    let quoted_http = shell_argument(http);
    let branch = shell_argument(branch);
    let new_repository = format!(
        "git init\ngit add .\ngit commit -m \"first commit\"\ngit branch -M {branch}\ngit remote add origin {quoted_http}\ngit push -u origin {branch}"
    );
    let existing_repository = format!(
        "git remote add origin {quoted_http}\ngit branch -M {branch}\ngit push -u origin {branch}"
    );
    rsx! {
        div { class: "repo-quick-setup",
            h3 { "Quick setup" }
            p { class: "repo-quick-setup-hint", "Consider adding a README, a .gitignore and a license to your first commit." }
            div { class: "repo-quick-setup-clone", {clone_choice("quick-setup", http, ssh)} }
            h4 { "Create a new repository on the command line" }
            pre { class: "repo-quick-setup-code", "{new_repository}" }
            h4 { "Push an existing repository from the command line" }
            pre { class: "repo-quick-setup-code", "{existing_repository}" }
        }
    }
}

/// The clone URLs of a repository. With an SSH URL too, a segmented HTTPS and SSH choice shows one
/// URL field at a time. The choice is a radio group, so it works without scripts. `prefix` keeps the
/// ids unique when a page shows two of these.
fn clone_choice(prefix: &str, http: &str, ssh: &str) -> Element {
    if ssh.is_empty() {
        return rsx! { div { class: "repo-clone-https", {clone_field("HTTPS", http)} } };
    }
    let https_id = format!("{prefix}-https");
    let ssh_id = format!("{prefix}-ssh");
    rsx! {
        input { r#type: "radio", id: "{https_id}", name: "{prefix}-protocol", class: "repo-sr-only repo-clone-radio-https", checked: true }
        input { r#type: "radio", id: "{ssh_id}", name: "{prefix}-protocol", class: "repo-sr-only repo-clone-radio-ssh" }
        nav { class: "repo-view-switch", "aria-label": "Clone protocol",
            label { class: "repo-view-option repo-view-option-https", r#for: "{https_id}", "HTTPS" }
            label { class: "repo-view-option repo-view-option-ssh", r#for: "{ssh_id}", "SSH" }
        }
        div { class: "repo-clone-https", {clone_field("HTTPS", http)} }
        div { class: "repo-clone-ssh", {clone_field("SSH", ssh)} }
    }
}

/// A clone URL in a read-only field, with its copy icon inside the field.
fn clone_field(protocol: &str, url: &str) -> Element {
    rsx! {
        div { class: "repo-clone-row",
            input { readonly: true, "value": url.to_owned(), "aria-label": format!("{protocol} clone URL"), class: "repo-clone-url" }
            CopyButton { text: url.to_owned(), label: format!("Copy {protocol} URL"), icon_only: true }
        }
    }
}

pub(in crate::webui::pages::repos) fn breadcrumbs(
    repo: &RepoChrome,
    reference: &str,
    path: &str,
    file: bool,
) -> Element {
    let base = repo.base();
    let parts: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    rsx! {
        nav { class: "repo-breadcrumbs", "aria-label": "File path",
            if !path.is_empty() { NavLink { href: file_url(&base, "tree", reference, ""), "{repo.name}" } }
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

#[cfg(test)]
mod tests {
    use super::website_host;

    #[test]
    fn setup_arguments_are_single_shell_words() {
        assert_eq!(
            super::shell_argument("main;touch${IFS}proof;#"),
            "'main;touch${IFS}proof;#'"
        );
        assert_eq!(
            super::shell_argument("https://example/a'b.git"),
            "'https://example/a'\\''b.git'"
        );
    }

    #[test]
    fn the_about_link_names_the_host_of_its_address() {
        assert_eq!(website_host("https://example.com/docs?x=1"), "example.com");
        assert_eq!(website_host("http://example.org"), "example.org");
        assert_eq!(website_host("https://"), "https://");
    }
}

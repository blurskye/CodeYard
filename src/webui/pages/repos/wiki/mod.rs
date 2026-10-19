//! The repository wiki: pages, their editor, history, revisions and compare. The pages render on the
//! server and work without JavaScript. The adapters decide who may see or change what.
mod diff;
mod links;
#[cfg(test)]
mod tests;

pub use diff::{line_diff, DiffLine, LineKind};
pub use links::expand;

use crate::webui::components::{compact_date, confirm_dialog, Markdown};
use crate::webui::navigation::NavLink;
use crate::webui::pages::repos::chrome::{chrome, csrf, icon, RepoChrome};
use crate::webui::pages::repos::issues::editor::MarkdownEditor;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

/// The address of a page: lowercase letters, digits and underscores, with runs of spaces and
/// hyphens joined by one hyphen. `None` when the text has none of them.
pub fn slug_for(title: &str) -> Option<String> {
    let mut slug = String::new();
    for ch in title.trim().chars() {
        if ch.is_alphanumeric() || ch == '_' {
            slug.extend(ch.to_lowercase());
        } else if (ch.is_whitespace() || ch == '-') && !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    (!slug.is_empty()).then_some(slug)
}

/// A page's content as readers see it. `revision` is its head number, which edits send back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageItem {
    pub slug: String,
    pub title: String,
    pub body: String,
    pub revision: i64,
    pub author: String,
    pub saved_at: String,
    pub message: String,
}

/// One saved version of a page, as the history lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RevisionItem {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub message: String,
    pub author: String,
    pub saved_at: String,
}

/// A row of the page list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListItem {
    pub slug: String,
    pub title: String,
    pub updated_at: String,
}

/// The fields of the editor as the writer typed them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DraftFields {
    pub title: String,
    pub body: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Page {
    /// A page that exists. `known` lists every page address, for the wiki links.
    Show {
        page: PageItem,
        sidebar: Option<PageItem>,
        footer: Option<PageItem>,
        known: Vec<String>,
        can_write: bool,
        can_admin: bool,
    },
    /// No page has this address. `title` is the name the link asked for.
    Missing {
        title: String,
        can_write: bool,
        /// The repository has no pages yet, so the Home page is the first to create.
        empty_wiki: bool,
    },
    List {
        items: Vec<ListItem>,
        query: String,
        can_write: bool,
    },
    /// The editor. `editing` names the page and the head revision the writer opened.
    Form {
        editing: Option<(String, i64)>,
        draft: DraftFields,
        error: Option<String>,
    },
    History {
        slug: String,
        title: String,
        revisions: Vec<RevisionItem>,
    },
    Revision {
        slug: String,
        revision: RevisionItem,
        is_head: bool,
        known: Vec<String>,
    },
    Compare {
        slug: String,
        old: RevisionItem,
        new: RevisionItem,
        diff: Vec<DiffLine>,
    },
}

pub fn render(repo: &RepoChrome, page: Page) -> Element {
    let body = match page {
        Page::Show {
            page,
            sidebar,
            footer,
            known,
            can_write,
            can_admin,
        } => show(
            repo,
            &page,
            sidebar.as_ref(),
            footer.as_ref(),
            &known,
            can_write,
            can_admin,
        ),
        Page::Missing {
            title,
            can_write,
            empty_wiki,
        } => missing(repo, &title, can_write, empty_wiki),
        Page::List {
            items,
            query,
            can_write,
        } => list(repo, &items, &query, can_write),
        Page::Form {
            editing,
            draft,
            error,
        } => form(repo, editing.as_ref(), &draft, error.as_deref()),
        Page::History {
            slug,
            title,
            revisions,
        } => history(repo, &slug, &title, &revisions),
        Page::Revision {
            slug,
            revision,
            is_head,
            known,
        } => revision_view(repo, &slug, &revision, is_head, &known),
        Page::Compare {
            slug,
            old,
            new,
            diff,
        } => compare(repo, &slug, &old, &new, &diff),
    };
    chrome(repo, body)
}

fn page_url(repo: &RepoChrome, slug: &str) -> String {
    format!(
        "{}/wiki/{}",
        repo.base(),
        crate::webui::encode_component(slug)
    )
}

/// The wiki's own navigation, shared by every wiki page.
fn wiki_nav(repo: &RepoChrome, can_write: bool) -> Element {
    let base = repo.base();
    rsx! {
        nav { class: "wiki-nav", aria_label: "Wiki",
            NavLink { class: "btn btn-secondary btn-sm", href: format!("{base}/wiki"), {icon("wiki")} " Home" }
            NavLink { class: "btn btn-secondary btn-sm", href: format!("{base}/wiki/_pages"), {icon("files")} " All pages" }
            if can_write {
                NavLink { class: "btn btn-primary btn-sm", href: format!("{base}/wiki/_new"), {icon("plus")} " New page" }
            }
        }
    }
}

fn last_edit(item: &PageItem, slug: &str, base: &str) -> Element {
    rsx! {
        p { class: "collaboration-muted wiki-meta",
            "Last edited "
            time { datetime: item.saved_at.clone(), "{compact_date(&item.saved_at)}" }
            " by {item.author}"
            if !item.message.is_empty() { " · {item.message}" }
            " · "
            NavLink { href: format!("{base}/wiki/{slug}/history"), "History" }
        }
    }
}

fn show(
    repo: &RepoChrome,
    page: &PageItem,
    sidebar: Option<&PageItem>,
    footer: Option<&PageItem>,
    known: &[String],
    can_write: bool,
    can_admin: bool,
) -> Element {
    let base = repo.base();
    let slug = crate::webui::encode_component(&page.slug);
    let delete_id = "delete-wiki-page";
    let content = expand(&page.body, &base, known);
    rsx! {
        section { class: "collaboration-page wiki-page", aria_label: "{page.title}",
            {wiki_nav(repo, can_write)}
            div { class: "wiki-layout",
                article { class: "wiki-main",
                    header { class: "repo-page-heading",
                        h2 { "{page.title}" }
                        div { class: "wiki-actions",
                            if can_write {
                                NavLink { class: "btn btn-secondary btn-sm", href: format!("{base}/wiki/{slug}/edit"), title: "Edit this page", {icon("edit")} " Edit" }
                            }
                            if can_admin {
                                a { class: "btn btn-danger btn-sm", href: format!("#{delete_id}"), title: "Delete this page", {icon("trash")} " Delete" }
                            }
                        }
                    }
                    div { class: "collaboration-markdown wiki-body",
                        if page.body.trim().is_empty() {
                            p { class: "collaboration-muted", "This page is empty." }
                        } else {
                            Markdown { content: content }
                        }
                    }
                    {last_edit(page, &page.slug, &base)}
                    if let Some(footer) = footer {
                        footer { class: "wiki-footer collaboration-markdown", aria_label: "Wiki footer",
                            Markdown { content: expand(&footer.body, &base, known) }
                        }
                    }
                }
                if let Some(sidebar) = sidebar {
                    aside { class: "wiki-sidebar collaboration-markdown", aria_label: "Wiki sidebar",
                        Markdown { content: expand(&sidebar.body, &base, known) }
                    }
                }
            }
            if can_admin {
                {confirm_dialog(
                    Some(delete_id),
                    "Delete page",
                    &page_url(repo, &page.slug),
                    &format!("{base}/wiki/{slug}/delete"),
                    rsx! { {csrf(&repo.csrf_token)} p { "Delete “{page.title}” and its history? Other pages' links to it will show as missing." } },
                    "Delete page",
                )}
            }
        }
    }
}

fn missing(repo: &RepoChrome, title: &str, can_write: bool, empty_wiki: bool) -> Element {
    let base = repo.base();
    let target = crate::webui::encode_component(title);
    rsx! {
        section { class: "collaboration-page wiki-page", aria_label: "Wiki page not found",
            {wiki_nav(repo, can_write)}
            header { class: "repo-page-heading",
                h2 { "{title}" }
            }
            if empty_wiki {
                p { class: "collaboration-muted wiki-empty",
                    "This wiki has no pages yet. Pages are stored in Codeyard itself, not in a Git repository, so the wiki cannot be cloned or pushed in this version."
                }
            } else {
                p { class: "collaboration-muted", "There is no page called “{title}” yet." }
            }
            if can_write {
                NavLink { class: "btn btn-primary", href: format!("{base}/wiki/_new?title={target}"), {icon("plus")} " Create this page" }
            }
        }
    }
}

fn list(repo: &RepoChrome, items: &[ListItem], query: &str, can_write: bool) -> Element {
    let base = repo.base();
    rsx! {
        section { class: "collaboration-page wiki-page", aria_label: "All pages",
            {wiki_nav(repo, can_write)}
            header { class: "repo-page-heading",
                h2 { {icon("files")} " All pages" }
            }
            form { method: "get", action: format!("{base}/wiki/_pages"), class: "wiki-search", role: "search",
                input { r#type: "search", name: "q", class: "collaboration-input", value: query.to_owned(), placeholder: "Find a page", aria_label: "Find a page" }
                button { class: "btn btn-secondary", r#type: "submit", title: "Search pages", {icon("search")} " Find" }
            }
            if items.is_empty() {
                p { class: "collaboration-muted",
                    if query.is_empty() { "No pages yet." } else { "No page matches “{query}”." }
                }
            } else {
                ul { class: "wiki-page-list",
                    for item in items {
                        li { class: "wiki-page-row",
                            NavLink { class: "wiki-page-title", href: format!("{base}/wiki/{}", crate::webui::encode_component(&item.slug)), "{item.title}" }
                            span { class: "collaboration-muted", time { datetime: item.updated_at.clone(), "{compact_date(&item.updated_at)}" } }
                        }
                    }
                }
            }
        }
    }
}

fn form(
    repo: &RepoChrome,
    editing: Option<&(String, i64)>,
    draft: &DraftFields,
    error: Option<&str>,
) -> Element {
    let base = repo.base();
    let (action, heading, cancel) = match editing {
        Some((slug, _)) => (
            format!("{base}/wiki/{}/edit", crate::webui::encode_component(slug)),
            "Edit page",
            format!("{base}/wiki/{}", crate::webui::encode_component(slug)),
        ),
        None => (
            format!("{base}/wiki/_new"),
            "New page",
            format!("{base}/wiki/_pages"),
        ),
    };
    rsx! {
        section { class: "collaboration-page wiki-page wiki-form", aria_label: heading,
            header { class: "repo-page-heading",
                h2 { "{heading}" }
            }
            if let Some(error) = error {
                p { class: "collaboration-error", role: "alert", "{error}" }
            }
            form { method: "post", action, class: "releases-form", id: "wiki-form",
                {csrf(&repo.csrf_token)}
                if let Some((_, revision)) = editing {
                    input { r#type: "hidden", name: "revision", value: revision.to_string() }
                }
                div { class: "collaboration-form-fields",
                    label { class: "collaboration-field", "Title"
                        input { r#type: "text", name: "title", class: "collaboration-input", required: true, maxlength: "200", value: draft.title.clone() }
                    }
                    div { class: "collaboration-field", "Content"
                        MarkdownEditor { id: "wiki-body".to_owned(), initial: draft.body.clone(), rows: 16, required: false, initial_preview: false, placeholder: "Write the page in Markdown. Link to another page with [[Page Name]].".to_owned() }
                    }
                    label { class: "collaboration-field", "Edit message (optional)"
                        input { r#type: "text", name: "message", class: "collaboration-input", maxlength: "500", value: draft.message.clone(), placeholder: "What changed" }
                    }
                }
                div { class: "releases-form-actions",
                    a { class: "btn btn-secondary", href: cancel, "Cancel" }
                    button { class: "btn btn-primary", r#type: "submit", {icon("check")} " Save page" }
                }
            }
        }
    }
}

fn history(repo: &RepoChrome, slug: &str, title: &str, revisions: &[RevisionItem]) -> Element {
    let base = repo.base();
    let address = crate::webui::encode_component(slug);
    rsx! {
        section { class: "collaboration-page wiki-page", aria_label: "History of {title}",
            {wiki_nav(repo, false)}
            header { class: "repo-page-heading",
                div {
                    NavLink { class: "collaboration-milestone-back", href: format!("{base}/wiki/{address}"), "← {title}" }
                    h2 { {icon("commits")} " History" }
                }
            }
            ul { class: "wiki-revisions",
                for revision in revisions {
                    li { class: "wiki-revision-row",
                        div { class: "wiki-revision-main",
                            NavLink { class: "wiki-page-title", href: format!("{base}/wiki/{address}/revisions/{}", revision.number), "{revision.title}" }
                            span { class: "collaboration-muted",
                                "revision {revision.number} · "
                                time { datetime: revision.saved_at.clone(), "{compact_date(&revision.saved_at)}" }
                                " by {revision.author}"
                                if !revision.message.is_empty() { " · {revision.message}" }
                            }
                        }
                        if revision.number > 1 {
                            NavLink { class: "btn btn-secondary btn-sm", href: format!("{base}/wiki/{address}/compare/{}/{}", revision.number - 1, revision.number), title: "Compare with the revision before", {icon("branches")} " Compare" }
                        }
                    }
                }
            }
        }
    }
}

fn revision_view(
    repo: &RepoChrome,
    slug: &str,
    revision: &RevisionItem,
    is_head: bool,
    known: &[String],
) -> Element {
    let base = repo.base();
    let address = crate::webui::encode_component(slug);
    rsx! {
        section { class: "collaboration-page wiki-page", aria_label: "Revision {revision.number}",
            {wiki_nav(repo, false)}
            header { class: "repo-page-heading",
                div {
                    NavLink { class: "collaboration-milestone-back", href: format!("{base}/wiki/{address}/history"), "← History" }
                    h2 { "{revision.title}" }
                    p { class: "collaboration-muted wiki-meta",
                        "Revision {revision.number} · "
                        time { datetime: revision.saved_at.clone(), "{compact_date(&revision.saved_at)}" }
                        " by {revision.author}"
                        if !revision.message.is_empty() { " · {revision.message}" }
                    }
                }
            }
            if !is_head {
                p { class: "wiki-old-banner", role: "status", "This is an older revision. " NavLink { href: format!("{base}/wiki/{address}"), "View the current page" } }
            }
            div { class: "collaboration-markdown wiki-body",
                if revision.body.trim().is_empty() {
                    p { class: "collaboration-muted", "This revision is empty." }
                } else {
                    Markdown { content: expand(&revision.body, &base, known) }
                }
            }
        }
    }
}

fn compare(
    repo: &RepoChrome,
    slug: &str,
    old: &RevisionItem,
    new: &RevisionItem,
    diff: &[DiffLine],
) -> Element {
    let base = repo.base();
    let address = crate::webui::encode_component(slug);
    rsx! {
        section { class: "collaboration-page wiki-page", aria_label: "Compare revisions",
            {wiki_nav(repo, false)}
            header { class: "repo-page-heading",
                div {
                    NavLink { class: "collaboration-milestone-back", href: format!("{base}/wiki/{address}/history"), "← History" }
                    h2 { "Revision {old.number} to {new.number}" }
                }
            }
            if diff.iter().all(|line| line.kind == LineKind::Same) {
                p { class: "collaboration-muted", "The two revisions have the same text." }
            }
            div { class: "wiki-diff", role: "region", aria_label: "Changes",
                for line in diff {
                    div { class: diff_class(line.kind),
                        span { class: "wiki-diff-sign", aria_hidden: "true", {diff_sign(line.kind)} }
                        span { class: "wiki-diff-text", "{line.text}" }
                    }
                }
            }
        }
    }
}

fn diff_class(kind: LineKind) -> &'static str {
    match kind {
        LineKind::Same => "wiki-diff-line",
        LineKind::Added => "wiki-diff-line wiki-diff-add",
        LineKind::Removed => "wiki-diff-line wiki-diff-del",
    }
}

fn diff_sign(kind: LineKind) -> &'static str {
    match kind {
        LineKind::Same => " ",
        LineKind::Added => "+",
        LineKind::Removed => "-",
    }
}

//! The branch and tag menu of the repository pages. It lists both kinds of ref, narrows them as the
//! reader types, and links each name to the page the caller chose.
#[cfg(test)]
#[path = "ref_switcher_tests.rs"]
mod tests;

use super::chrome::icon;
use super::commits::history_url;
use super::ref_path::ref_switch_url;
use crate::webui::navigation::NavLink;
use dioxus::prelude::*;

/// Where the menu's names lead. Links are built only for the names the menu draws, so a long
/// path is not copied into one URL per ref.
#[derive(Clone, Debug, PartialEq)]
pub enum RefLinks {
    /// `path` on the chosen ref, or the ref's root when the path is missing there, so a ref
    /// switch keeps the reader where they are when it can.
    Files {
        base: String,
        kind: &'static str,
        path: String,
    },
    /// The history of `path`, keeping the history filters in `query`.
    History {
        base: String,
        path: String,
        query: String,
    },
}

impl RefLinks {
    fn href(&self, name: &str, tag: bool) -> String {
        match self {
            Self::Files { base, kind, path } => {
                let namespace = if tag { "refs/tags/" } else { "refs/heads/" };
                ref_switch_url(base, kind, &format!("{namespace}{name}"), path)
            }
            Self::History { base, path, query } => {
                let reference = if tag {
                    format!("refs/tags/{name}")
                } else {
                    format!("refs/heads/{name}")
                };
                format!("{}{query}", history_url(base, &reference, path))
            }
        }
    }
}

/// The current ref as a menu. Every name is a plain link, so the lists work without JavaScript.
/// The filter narrows them once the page runs in the browser. The branch named `default_branch`
/// carries a "default" badge; an empty name marks none.
#[component]
pub fn RefSwitcher(
    base: String,
    current: String,
    branches: Vec<String>,
    tags: Vec<String>,
    links: RefLinks,
    #[props(default = String::new())] default_branch: String,
) -> Element {
    let mut filter = use_signal(String::new);
    let query = filter().trim().to_lowercase();
    let shown_branches = matching(&branches, &query);
    let shown_tags = matching(&tags, &query);
    let nothing_shown = shown_branches.is_empty() && shown_tags.is_empty();
    let display = current
        .strip_prefix("refs/tags/")
        .or_else(|| current.strip_prefix("refs/heads/"))
        .unwrap_or(&current);
    rsx! {
        details { class: "native-menu repo-menu repo-ref-menu",
            summary { class: "btn btn-secondary", {icon("branches")} span { class: "repo-ref-name", title: "{display}", "{display}" } {icon("chevron")} }
            div { class: "repo-menu-panel repo-ref-panel",
                h2 { class: "repo-menu-heading", "Switch branches or tags" }
                div { class: "repo-ref-filter-box",
                    label { class: "sr-only", r#for: "repo-ref-filter", "Find a branch or tag" }
                    input { id: "repo-ref-filter", class: "repo-ref-filter", r#type: "search", placeholder: "Find a branch or tag", autocomplete: "off",
                        oninput: move |event| filter.set(event.value()) }
                }
                {ref_group("Branches", &shown_branches, &current, Some(default_branch.as_str()), &links)}
                {ref_group("Tags", &shown_tags, &current, None, &links)}
                if nothing_shown { p { class: "repo-ref-empty", "No branch or tag matches." } }
                div { class: "repo-menu-footer", NavLink { href: format!("{base}/branches"), "View all branches" } NavLink { href: format!("{base}/tags"), "View all tags" } }
            }
        }
    }
}

/// Most names the menu draws at once. The filter reaches the rest.
const MAX_REF_OPTIONS: usize = 50;

/// The names that contain `query`, which is already trimmed and lower-cased.
fn matching<'a>(names: &'a [String], query: &str) -> Vec<&'a String> {
    names
        .iter()
        .filter(|name| name.to_lowercase().contains(query))
        .take(MAX_REF_OPTIONS)
        .collect()
}

/// One kind of ref under its heading. Nothing is drawn for an empty group. `default_branch` is the
/// name to mark with the default badge, and it is `None` for tags.
fn ref_group(
    title: &str,
    names: &[&String],
    current: &str,
    default_branch: Option<&str>,
    links: &RefLinks,
) -> Element {
    if names.is_empty() {
        return rsx! {};
    }
    let count = names.len();
    let tag = title == "Tags";
    let selected = |name: &str| {
        if tag {
            current == format!("refs/tags/{name}")
        } else {
            current == name || current == format!("refs/heads/{name}")
        }
    };
    rsx! {
        section { class: "repo-ref-group",
            h3 { class: "repo-ref-group-title", "{title}" span { class: "repo-ref-group-count", "{count}" } }
            ul { class: "repo-ref-list",
                for name in names {
                    li { key: "{name}",
                        NavLink { class: (if selected(name) { "repo-menu-item repo-ref-selected" } else { "repo-menu-item" }).to_owned(), href: links.href(name, tag),
                            span { class: "repo-ref-check", "aria-hidden": "true", if selected(name) { {icon("check")} } }
                            span { class: "repo-ref-option-name", title: "{name}", "{name}" }
                            if default_branch == Some(name.as_str()) {
                                span { class: "repo-label-neutral repo-ref-default", "default" }
                            }
                        }
                    }
                }
            }
        }
    }
}

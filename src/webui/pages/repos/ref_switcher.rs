//! The branch and tag menu of the repository pages. It lists both kinds of ref, narrows them as the
//! reader types, and links each name to the page the caller chose.
#[cfg(test)]
#[path = "ref_switcher_tests.rs"]
mod tests;

use super::chrome::icon;
use super::ref_path::ref_switch_url;
use crate::webui::navigation::NavLink;
use dioxus::prelude::*;

/// A branch or tag in the menu, with the page that opens it.
#[derive(Clone, Debug, PartialEq)]
pub struct RefOption {
    pub name: String,
    pub href: String,
}

/// The current ref as a menu. Every name is a plain link, so the lists work without JavaScript.
/// The filter narrows them once the page runs in the browser. The branch named `default_branch`
/// carries a "default" badge; an empty name marks none.
#[component]
pub fn RefSwitcher(
    base: String,
    current: String,
    branches: Vec<RefOption>,
    tags: Vec<RefOption>,
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
                {ref_group("Branches", &shown_branches, &current, Some(default_branch.as_str()))}
                {ref_group("Tags", &shown_tags, &current, None)}
                if nothing_shown { p { class: "repo-ref-empty", "No branch or tag matches." } }
                div { class: "repo-menu-footer", NavLink { href: format!("{base}/branches"), "View all branches" } NavLink { href: format!("{base}/tags"), "View all tags" } }
            }
        }
    }
}

/// Most names the menu draws at once. The filter reaches the rest.
const MAX_REF_OPTIONS: usize = 50;

/// One option per name. Each option links to `path` on that ref, or to the ref's root when the
/// path is missing there, so a ref switch keeps the reader where they are when it can.
pub fn ref_options(
    base: &str,
    kind: &str,
    path: &str,
    names: &[String],
    prefix: &str,
) -> Vec<RefOption> {
    names
        .iter()
        .map(|name| RefOption {
            name: name.clone(),
            href: ref_switch_url(
                base,
                kind,
                &format!(
                    "{}{name}",
                    if prefix.is_empty() {
                        "refs/heads/"
                    } else {
                        prefix
                    }
                ),
                path,
            ),
        })
        .collect()
}

/// The options whose names contain `query`, which is already trimmed and lower-cased.
fn matching<'a>(options: &'a [RefOption], query: &str) -> Vec<&'a RefOption> {
    options
        .iter()
        .filter(|option| option.name.to_lowercase().contains(query))
        .take(MAX_REF_OPTIONS)
        .collect()
}

/// One kind of ref under its heading. Nothing is drawn for an empty group. `default_branch` is the
/// name to mark with the default badge, and it is `None` for tags.
fn ref_group(
    title: &str,
    options: &[&RefOption],
    current: &str,
    default_branch: Option<&str>,
) -> Element {
    if options.is_empty() {
        return rsx! {};
    }
    let count = options.len();
    let selected = |name: &str| {
        if title == "Tags" {
            current == format!("refs/tags/{name}")
        } else {
            current == name || current == format!("refs/heads/{name}")
        }
    };
    rsx! {
        section { class: "repo-ref-group",
            h3 { class: "repo-ref-group-title", "{title}" span { class: "repo-ref-group-count", "{count}" } }
            ul { class: "repo-ref-list",
                for option in options {
                    li { key: "{option.name}",
                        NavLink { class: (if selected(&option.name) { "repo-menu-item repo-ref-selected" } else { "repo-menu-item" }).to_owned(), href: option.href.clone(),
                            span { class: "repo-ref-check", "aria-hidden": "true", if selected(&option.name) { {icon("check")} } }
                            span { class: "repo-ref-option-name", title: "{option.name}", "{option.name}" }
                            if default_branch == Some(option.name.as_str()) {
                                span { class: "repo-label-neutral repo-ref-default", "default" }
                            }
                        }
                    }
                }
            }
        }
    }
}

//! "Go to file": every file path at one revision. The server applies the `q` query when it
//! renders; after hydration the same box filters the list as the reader types, with the same rule.
use crate::webui::pages::repos::{
    chrome::{chrome, icon, RepoChrome},
    ref_path::file_url,
};
use crate::webui::{components::count_label, navigation::NavLink};
use dioxus::prelude::*;

/// Most paths one listing shows. The backend cuts a larger tree to this many. The notice
/// below names this number, so change both together.
pub const MAX_LISTED_FILES: usize = 10_000;

/// Every file path at a revision, sorted. `truncated` says the tree had more paths than were kept.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FindPage {
    pub reference: String,
    pub paths: Vec<String>,
    pub truncated: bool,
    pub query: String,
}

/// Whether `path` matches `query`: every whitespace-separated term must appear in the path,
/// ignoring case. An empty query matches every path.
pub fn path_matches(path: &str, query: &str) -> bool {
    let path = path.to_lowercase();
    query
        .split_whitespace()
        .all(|term| path.contains(&term.to_lowercase()))
}

pub fn find(repo: &RepoChrome, page: &FindPage) -> Element {
    let base = repo.base();
    // A new query or revision is a new page, so the box starts from the server's value.
    chrome(
        repo,
        rsx! {
            section { class: "repo-find min-w-0", "aria-label": "Go to file",
                div { key: "{page.reference}?{page.query}", class: "space-y-4",
                    header { class: "repo-page-heading",
                        div { h2 { "Go to file on " NavLink { href: file_url(&base, "tree", &page.reference, ""), {icon("branches")} "{page.reference}" } } }
                    }
                    FindFiles { base: base.clone(), page: page.clone() }
                }
            }
        },
    )
}

#[component]
fn FindFiles(base: String, page: FindPage) -> Element {
    let initial = page.query.clone();
    let mut filter = use_signal(move || initial);
    let current = filter();
    let filtering = !current.trim().is_empty();
    let matches: Vec<&String> = page
        .paths
        .iter()
        .filter(|path| path_matches(path, &current))
        .collect();
    // Enter opens the first match once the page runs in the browser.
    let highlighted = matches
        .first()
        .filter(|_| filtering)
        .map(|path| file_url(&base, "blob", &page.reference, path));
    let summary = count_label(matches.len(), "file", "files");
    let find_url = file_url(&base, "find", &page.reference, "");
    rsx! {
        form { class: "repo-find-form", method: "get", action: find_url, role: "search", "aria-label": "Go to file",
            label { class: "repo-sr-only", "for": "repo-find-query", "Find a file by name" }
            input { id: "repo-find-query", class: "repo-find-input", r#type: "search", name: "q", value: current.clone(), maxlength: "256", placeholder: "Go to file", autocomplete: "off",
                oninput: move |event| filter.set(event.value()),
                onkeydown: move |event| {
                    #[cfg(target_arch = "wasm32")]
                    {
                        if let Some(href) = &highlighted {
                            if event.key() == Key::Enter {
                                event.prevent_default();
                                if let Some(window) = web_sys::window() {
                                    let _ = window.location().assign(href);
                                }
                            }
                        }
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    let _ = (event, &highlighted);
                }
            }
        }
        p { class: "repo-find-summary", "{summary}" }
        if page.truncated { p { class: "repo-find-notice", "Showing the first 10,000 files." } }
        if matches.is_empty() {
            div { class: "repo-empty panel", {icon("search")} h3 { "No files match" } p { "Try part of the file name or the folder path." } }
        } else {
            div { class: "repo-file-browser border border-border rounded-md bg-card",
                table { class: "repo-file-table repo-find-table w-full border-collapse text-sm",
                    caption { class: "repo-sr-only", "Matching files" }
                    tbody {
                        for (index, path) in matches.iter().enumerate() {
                            tr { key: "{path}", class: if index == 0 && filtering { "repo-find-first" } else { "" },
                                td { class: "repo-file-name",
                                    div { class: "repo-file-name-inner",
                                        {icon("file")}
                                        NavLink { href: file_url(&base, "blob", &page.reference, path), "{path}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

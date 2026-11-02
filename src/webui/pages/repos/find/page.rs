//! "Go to file": every file path at one revision. The server applies the `q` query when it
//! renders; after hydration the same box filters the list as the reader types, with the same rule.
use crate::webui::pages::repos::{
    chrome::{chrome, icon, RepoChrome},
    ref_path::file_url,
};
use crate::webui::{components::count_label, navigation::NavLink};
use dioxus::prelude::*;

/// Most paths one listing carries. The backend cuts a larger tree to this many.
pub const MAX_LISTED_FILES: usize = 10_000;

/// Most matching rows the page draws. The reader narrows the query to reach the rest.
const MAX_SHOWN_FILES: usize = 100;

/// Every file path at a revision, sorted. `truncated` says the tree had more paths than were kept.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FindPage {
    pub reference: String,
    pub paths: Vec<String>,
    pub truncated: bool,
    pub query: String,
}

/// The whitespace-separated terms of a query: lowercased, deduplicated and cut to 256 characters.
pub fn query_terms(query: &str) -> Vec<String> {
    let query: String = query.chars().take(256).collect();
    let mut terms: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
    terms.sort_unstable();
    terms.dedup();
    terms
}

/// Whether every term appears in `path`, ignoring case. No terms match every path.
fn matches_terms(path: &str, terms: &[String]) -> bool {
    let path = path.to_lowercase();
    terms.iter().all(|term| path.contains(term))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_normalization_is_bounded_case_folded_and_deduplicated() {
        assert_eq!(query_terms("A a B b"), ["a", "b"]);
        assert_eq!(query_terms(&"a ".repeat(2000)), ["a"]);
        assert_eq!(query_terms(&"é".repeat(300))[0].chars().count(), 256);
        assert!(matches_terms("docs/AB", &query_terms("A b a")));
    }

    #[test]
    fn path_filter_needs_every_term_and_ignores_case() {
        let matches = |path: &str, query: &str| matches_terms(path, &query_terms(query));
        assert!(matches("docs/Guide.md", ""));
        assert!(matches("docs/Guide.md", "  guide   DOCS "));
        assert!(matches("src/lib.rs", "lib.rs"));
        assert!(!matches("docs/Guide.md", "guide readme"));
        assert!(!matches("src/lib.rs", "lib.rs.bak"));
    }

    #[test]
    fn find_imports_the_native_field_when_it_mounts() {
        let mut dom = VirtualDom::new_with_props(
            |page: FindPage| rsx! { FindFiles { base: "/u/r".to_owned(), page } },
            FindPage {
                reference: "main".into(),
                query: "foo".into(),
                paths: vec!["foo.txt".into(), "bar.txt".into()],
                truncated: false,
            },
        );
        let edits = dom.rebuild_to_vec();
        assert!(edits.edits.iter().any(|edit| matches!(
            edit,
            dioxus::core::Mutation::NewEventListener {
                name,
                ..
            } if name == "mounted"
        )));
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("value=\"foo\""));
        assert!(html.contains("foo.txt"));
        assert!(!html.contains("bar.txt"));
    }
}

#[component]
fn FindFiles(base: String, page: FindPage) -> Element {
    let initial = page.query.clone();
    let mut filter = use_signal(move || initial);
    let current = filter();
    let terms = query_terms(&current);
    let filtering = !terms.is_empty();
    let matches: Vec<&String> = page
        .paths
        .iter()
        .filter(|path| matches_terms(path, &terms))
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
            label { class: "sr-only", "for": "repo-find-query", "Find a file by name" }
            input { id: "repo-find-query", class: "repo-find-input", r#type: "search", name: "q", value: current.clone(), maxlength: "256", placeholder: "Go to file", autocomplete: "off",
                oninput: move |event| filter.set(event.value()),
                onmounted: move |event| {
                    #[cfg(target_arch = "wasm32")]
                    { use wasm_bindgen::JsCast;
                      if let Some(element) = event.data().downcast::<web_sys::Element>() {
                          if let Some(input) = element.dyn_ref::<web_sys::HtmlInputElement>() { filter.set(input.value()); }
                      }
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    let _ = event;
                },
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
        if page.truncated { p { class: "repo-find-notice", "Showing the first {MAX_LISTED_FILES} files." } }
        if matches.is_empty() {
            div { class: "repo-empty panel", {icon("search")} h3 { "No files match" } p { "Try part of the file name or the folder path." } }
        } else {
            div { class: "repo-file-browser border border-border rounded-md bg-card",
                table { class: "repo-file-table repo-find-table w-full border-collapse text-sm",
                    caption { class: "sr-only", "Matching files" }
                    tbody {
                        for (index, path) in matches.iter().take(MAX_SHOWN_FILES).enumerate() {
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
            if matches.len() > MAX_SHOWN_FILES {
                p { class: "repo-find-notice", "Showing the first {MAX_SHOWN_FILES} matches. Type more of the name or folder to narrow the list." }
            }
        }
    }
}

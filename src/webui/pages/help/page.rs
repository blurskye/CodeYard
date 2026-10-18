//! The help page: the keyboard shortcuts that exist, where the documentation is, and how to report
//! a problem.
use dioxus::prelude::*;

/// The project's public repository, from its Cargo metadata. Its docs and issue tracker are there.
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// The keys that work on every page, each with what it does. Each one is handled in the
/// header's key listener or in the menu code.
const SHORTCUTS: [(&str, &str); 3] = [
    (
        "/",
        "Focus the search box. On a repository page with a Files panel, it focuses the Go to file box.",
    ),
    (
        "?",
        "Open this help page, unless a text field has the cursor.",
    ),
    ("Esc", "Close an open menu."),
];

pub fn help_page() -> Element {
    let docs = format!("{REPOSITORY}/tree/main/docs");
    let issues = format!("{REPOSITORY}/issues");
    rsx! {
        div { class: "page-shell max-w-3xl py-8 pb-14",
            header { class: "mb-6 space-y-1",
                h1 { class: "text-2xl font-semibold tracking-tight", "Help" }
                p { class: "text-sm text-muted-foreground", "Keyboard shortcuts, the documentation, and how to report a problem." }
            }
            section { class: "panel mb-4 p-5", "aria-labelledby": "help-shortcuts",
                h2 { id: "help-shortcuts", class: "text-base font-semibold", "Keyboard shortcuts" }
                table { class: "mt-3 w-full text-sm",
                    thead {
                        tr { class: "text-left text-muted-foreground",
                            th { scope: "col", class: "w-24 py-2 font-medium", "Key" }
                            th { scope: "col", class: "py-2 font-medium", "Action" }
                        }
                    }
                    tbody {
                        for (key, action) in SHORTCUTS {
                            tr { class: "border-t border-border",
                                td { class: "py-2 pr-4",
                                    kbd { class: "rounded border border-border px-1.5 py-0.5 font-mono text-xs", "{key}" }
                                }
                                td { class: "py-2", "{action}" }
                            }
                        }
                    }
                }
            }
            section { class: "panel mb-4 p-5", "aria-labelledby": "help-docs",
                h2 { id: "help-docs", class: "text-base font-semibold", "Documentation" }
                p { class: "mt-2 text-sm text-muted-foreground", "The documentation is in the docs folder of the project repository." }
                a { href: docs, target: "_blank", rel: "noopener noreferrer", class: "focus-ring mt-3 inline-block text-sm text-primary hover:underline", "Read the documentation" }
            }
            section { class: "panel p-5", "aria-labelledby": "help-report",
                h2 { id: "help-report", class: "text-base font-semibold", "Report a problem" }
                p { class: "mt-2 text-sm text-muted-foreground", "A problem with Codeyard itself: open an issue in the project repository." }
                a { href: issues, target: "_blank", rel: "noopener noreferrer", class: "focus-ring mt-3 inline-block text-sm text-primary hover:underline", "Open an issue" }
                p { class: "mt-3 text-sm text-muted-foreground", "A problem on this server, such as a page that does not load or an account you cannot reach: tell the administrator of this server." }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_lists_the_shortcuts_the_app_handles_and_links_the_docs_and_the_issue_tracker() {
        let output = dioxus_ssr::render_element(help_page());
        assert!(output.contains("Keyboard shortcuts"), "{output}");
        for (key, action) in SHORTCUTS {
            assert!(output.contains(&format!(">{key}</kbd>")), "{key}: {output}");
            assert!(output.contains(action), "{key}: {output}");
        }
        assert!(
            output.contains(&format!("href=\"{REPOSITORY}/tree/main/docs\"")),
            "{output}"
        );
        assert!(
            output.contains(&format!("href=\"{REPOSITORY}/issues\"")),
            "{output}"
        );
        assert!(
            output.contains("tell the administrator of this server"),
            "{output}"
        );
    }
}

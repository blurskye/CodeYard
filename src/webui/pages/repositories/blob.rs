use super::{
    chrome::{chrome, icon},
    ref_path::{file_url, is_markdown_path, link_base},
    tree::breadcrumbs,
    RepoChrome,
};
use crate::webui::{
    components::{
        code::{language_of_path, themed_highlight_lines},
        count_label,
        references::References,
        CopyButton,
    },
    navigation::NavLink,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BlobPage {
    pub reference: String,
    pub path: String,
    pub content: String,
    pub binary: bool,
    pub size: u64,
    pub offset: u64,
    pub next: Option<u64>,
    pub previous: Option<u64>,
    pub truncated: bool,
    /// Text small enough for the blame view. Set by the server, which owns the limit.
    #[serde(default)]
    pub can_blame: bool,
    pub editable: bool,
    /// The reference is a branch, so a commit can remove the file. Binary files are deletable too.
    #[serde(default)]
    pub deletable: bool,
    /// Shows a Markdown file as its source (`?view=code`) rather than rendered.
    #[serde(default)]
    pub code_view: bool,
    /// The references the Markdown links; empty unless `renders_markdown`.
    #[serde(default)]
    pub references: References,
}

impl BlobPage {
    /// Whether the file has a rendered Markdown view, so the page offers Preview and Code.
    pub fn has_markdown_view(&self) -> bool {
        !self.binary && !self.truncated && is_markdown_path(&self.path)
    }

    /// Whether the view shows this file as Markdown rather than as code.
    pub fn renders_markdown(&self) -> bool {
        self.has_markdown_view() && !self.code_view
    }
}

pub fn blob(repo: &RepoChrome, page: &BlobPage) -> Element {
    let base = repo.base();
    let raw = file_url(&base, "raw", &page.reference, &page.path);
    let url = file_url(&base, "blob", &page.reference, &page.path);
    let blame = file_url(&base, "blame", &page.reference, &page.path);
    let name = page.path.rsplit('/').next().unwrap_or_default();
    let language = language_of_path(&page.path);
    let display = display_text(&page.content);
    let line_count = display.lines().count();
    // A preview that starts mid-file cannot say which line it starts at, so its lines get no
    // numbers and no #L anchors.
    let numbered = page.offset == 0;
    let line_numbers = if numbered {
        (1..=line_count)
            .map(|line| format!("<a href=\"#L{line}\">{line}</a>"))
            .collect::<String>()
    } else {
        String::new()
    };
    // Each line is its own block so the #L target can be highlighted, and the numbers line up
    // with the code without a newline between blocks.
    let code_lines = themed_highlight_lines(&display, language)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if numbered {
                format!(
                    "<span class=\"repo-code-line\" id=\"L{}\">{line}</span>",
                    index + 1
                )
            } else {
                format!("<span class=\"repo-code-line\">{line}</span>")
            }
        })
        .collect::<String>();
    chrome(
        repo,
        rsx! {
            div { class: "repo-blob-toolbar",
                div { class: "repo-blob-path",
                    NavLink { class: "btn btn-secondary repo-blob-ref".to_owned(), href: file_url(&base, "tree", &page.reference, ""), {icon("branches")} "{page.reference}" }
                    {breadcrumbs(repo, &page.reference, &page.path, true)}
                }
                if repo.can_write && page.editable {
                    NavLink { class: "btn btn-secondary".to_owned(), href: file_url(&base, "edit", &page.reference, &page.path), {icon("edit")} "Edit" }
                }
                if repo.can_write && page.deletable {
                    NavLink { class: "btn btn-secondary".to_owned(), href: file_url(&base, "delete", &page.reference, &page.path), {icon("trash")} "Delete" }
                }
            }
            if page.renders_markdown() {
                section { class: "repo-readme repo-blob-markdown border border-border rounded-md bg-card",
                    div { class: "repo-readme-header",
                        {icon("file")}
                        span { class: "repo-blob-name", "{name}" }
                        div { class: "repo-blob-actions",
                            {view_switch(&url, true)}
                            if page.can_blame { a { class: "btn btn-secondary", href: blame.clone(), "Blame" } }
                            a { class: "btn btn-secondary", href: raw.clone(), "Raw" }
                        }
                    }
                    div { class: "repo-readme-body", crate::webui::components::RepoMarkdown { content: page.content.clone(), base: link_base(&base, &page.reference, page.path.rsplit_once('/').map_or("", |(directory, _)| directory)), references: page.references.clone() } }
                }
            } else {
                div { class: "repo-blob border border-border rounded-md overflow-hidden",
                    div { class: "repo-blob-header border-b border-border",
                        div { class: "repo-blob-stats", if !page.binary { span { if page.truncated { "{line_count} preview lines" } else { {count_label(line_count, "line", "lines")} } } } span { "{page.size} B" } }
                        div { class: "repo-blob-actions",
                            if page.has_markdown_view() { {view_switch(&url, false)} }
                            if page.can_blame { a { class: "btn btn-secondary", href: blame.clone(), "Blame" } }
                            a { class: "btn btn-secondary", href: raw.clone(), "Raw" }
                            if !page.binary { CopyButton { text: page.content.clone(), label: (if page.truncated { "Copy preview" } else { "Copy code" }).to_owned() } }
                        }
                    }
                    if page.binary { p { class: "repo-binary", "Binary file. " a { href: raw.clone(), "Download the raw file" } } }
                    else {
                        div { class: "repo-code-body overflow-x-auto font-mono text-sm",
                            div { class: "repo-code-lines",
                                if !line_numbers.is_empty() { pre { class: "repo-line-numbers", dangerous_inner_html: line_numbers } }
                                pre { class: "shiki", code { dangerous_inner_html: code_lines } }
                            }
                        }
                    }
                }
            }
            if page.truncated { p { class: "text-sm text-muted-foreground", "Showing a bounded preview starting at byte {page.offset}. Raw contains the complete file." } }
            nav { class: "flex gap-4", "aria-label": "File preview",
                if let Some(offset) = page.previous { NavLink { href: format!("{url}?offset={offset}"), "Previous" } }
                if let Some(offset) = page.next { NavLink { href: format!("{url}?offset={offset}"), "Next" } }
            }
        },
    )
}

/// Preview and Code as plain links, so the switch works without JavaScript. Code is a query
/// flag on the same file, which the server reads when it builds the page.
fn view_switch(url: &str, preview: bool) -> Element {
    rsx! {
        nav { class: "repo-view-switch", "aria-label": "File view",
            a { class: if preview { "repo-view-option repo-view-option-active" } else { "repo-view-option" }, href: url.to_owned(), aria_current: if preview { "page" } else { "false" }, "Preview" }
            a { class: if preview { "repo-view-option" } else { "repo-view-option repo-view-option-active" }, href: format!("{url}?view=code"), aria_current: if preview { "false" } else { "page" }, "Code" }
        }
    }
}

/// A carriage return starts a new line on screen, so the code view shows it as a line
/// break. Raw and copy views keep the file's own bytes.
pub(super) fn display_text(content: &str) -> String {
    content.replace("\r\n", "\n").replace('\r', "\n")
}

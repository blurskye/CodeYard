use super::{
    chrome::{chrome, icon},
    commits::commit_date,
    ref_path::file_url,
    tree::breadcrumbs,
    RepoChrome,
};
use crate::webui::{
    components::{code::themed_highlight_lines, compact_date, count_label, Avatar},
    navigation::NavLink,
};
use dioxus::prelude::*;
use std::collections::HashMap;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BlamePage {
    pub reference: String,
    pub path: String,
    pub content: String,
    pub size: u64,
    pub hunks: Vec<BlameHunk>,
}

/// One run of consecutive lines and the commit that last changed them.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BlameHunk {
    pub first_line: usize,
    pub line_count: usize,
    pub sha: String,
    pub author: String,
    /// RFC 3339, as commit dates are given elsewhere.
    pub author_date: String,
    pub summary: String,
}

/// One source line. The hunk is set on the first line of its run only.
struct BlameRow<'a> {
    number: usize,
    code: String,
    hunk: Option<&'a BlameHunk>,
    /// Runs take turns at a lighter shade, so the boundary between two runs shows.
    shaded: bool,
}

pub fn blame(repo: &RepoChrome, page: &BlamePage) -> Element {
    let base = repo.base();
    let url = file_url(&base, "blob", &page.reference, &page.path);
    let raw = file_url(&base, "raw", &page.reference, &page.path);
    let language = page.path.rsplit('.').next().unwrap_or("text");
    // Git counts lines by newline alone, so a carriage return is dropped rather than shown as
    // a line break inside a table cell.
    let text = page.content.replace('\r', "");
    let rows = blame_rows(&page.hunks, themed_highlight_lines(&text, language));
    let line_count = rows.len();
    chrome(
        repo,
        rsx! {
            div { class: "repo-blob-toolbar",
                div { class: "repo-blob-path",
                    NavLink { class: "btn btn-secondary repo-blob-ref".to_owned(), href: file_url(&base, "tree", &page.reference, ""), {icon("branches")} "{page.reference}" }
                    {breadcrumbs(repo, &page.reference, &page.path, true)}
                }
            }
            section { class: "repo-blob repo-blame border border-border rounded-md overflow-hidden",
                div { class: "repo-blob-header border-b border-border",
                    div { class: "repo-blob-stats", span { {count_label(line_count, "line", "lines")} } span { "{page.size} B" } }
                    div { class: "repo-blob-actions",
                        a { class: "btn btn-secondary", href: url, "Code" }
                        a { class: "btn btn-secondary", href: raw, "Raw" }
                    }
                }
                div { class: "repo-blame-scroll",
                    table { class: "repo-blame-table",
                        tbody {
                            for row in rows {
                                tr { key: "{row.number}", class: if row.shaded { "repo-blame-row repo-blame-shaded" } else { "repo-blame-row" },
                                    if let Some(hunk) = row.hunk {
                                        td { class: "repo-blame-hunk", rowspan: "{hunk.line_count}",
                                            div { class: "repo-blame-commit",
                                                Avatar { name: hunk.author.clone(), size: 20 }
                                                div { class: "repo-blame-who",
                                                    span { class: "repo-blame-author", "{hunk.author}" }
                                                    span { class: "repo-blame-meta",
                                                        span { class: "repo-blame-date", {commit_date(&hunk.author_date)} }
                                                        span { class: "repo-blame-sep", "aria-hidden": "true", "·" }
                                                        a { class: "repo-blame-sha", href: format!("{base}/commit/{}", hunk.sha), title: "{hunk.author} · {compact_date(&hunk.author_date)} · {hunk.summary}", "{short_sha(&hunk.sha)}" }
                                                    }
                                                }
                                            }
                                            span { class: "repo-blame-summary", title: "{hunk.summary}", "{hunk.summary}" }
                                        }
                                    }
                                    td { class: "repo-blame-number", id: "L{row.number}", a { href: "#L{row.number}", "{row.number}" } }
                                    td { class: "repo-blame-code", pre { class: "shiki", code { dangerous_inner_html: row.code } } }
                                }
                            }
                        }
                    }
                }
            }
        },
    )
}

/// Pairs each source line with the hunk that starts on it. Lines outside any hunk get none.
fn blame_rows<'a>(hunks: &'a [BlameHunk], code: Vec<String>) -> Vec<BlameRow<'a>> {
    let starts: HashMap<usize, &BlameHunk> =
        hunks.iter().map(|hunk| (hunk.first_line, hunk)).collect();
    let mut shaded = false;
    code.into_iter()
        .enumerate()
        .map(|(index, code)| {
            let number = index + 1;
            let hunk = starts.get(&number).copied();
            if hunk.is_some() {
                shaded = !shaded;
            }
            BlameRow {
                number,
                code,
                hunk,
                shaded,
            }
        })
        .collect()
}

fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

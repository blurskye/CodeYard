use crate::webui::components::{code::themed_highlight_lines, compact_date, Avatar};
use crate::webui::pages::repos::{
    blob::{
        file_downloads, file_layout, file_stats, more_menu, title_row, view_switch, FileView,
        FolderListing,
    },
    chrome::{chrome, RepoChrome},
    commits::{commit_date, latest_commit_bar, Commit},
    ref_path::file_url,
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
    /// The root and each folder on the way to the file, for the Files panel.
    #[serde(default)]
    pub folders: Vec<FolderListing>,
    /// The last commit that changed the file, shown above it.
    #[serde(default)]
    pub latest_commit: Option<Commit>,
    /// The file at the commit it was read at, so the link keeps showing this file.
    #[serde(default)]
    pub permalink: String,
    /// Every branch and every tag, sorted, for the ref menu.
    #[serde(default)]
    pub branches: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The reference is a branch, so a commit can remove the file.
    #[serde(default)]
    pub deletable: bool,
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
    let blame_url = file_url(&base, "blame", &page.reference, &page.path);
    let name = page.path.rsplit('/').next().unwrap_or_default();
    let language = page.path.rsplit('.').next().unwrap_or("text");
    // Git counts lines by newline alone, so a carriage return is dropped rather than shown as
    // a line break inside a table cell.
    let text = page.content.replace('\r', "");
    let stats = file_stats(&text, page.size);
    let rows = blame_rows(&page.hunks, themed_highlight_lines(&text, language));
    let delete_link = (repo.can_write && page.deletable).then_some(file_url(
        &base,
        "delete",
        &page.reference,
        &page.path,
    ));
    let content = rsx! {
        {title_row(repo, &page.reference, &page.path, true, rsx! {})}
        section { class: "repo-blob repo-blame border border-border rounded-md bg-card",
            if let Some(commit) = &page.latest_commit {
                {latest_commit_bar(&base, &page.reference, &page.path, commit)}
            }
            div { class: "repo-blob-header",
                div { class: "repo-blob-heading",
                    {view_switch(&[FileView { label: "Code", href: url, active: false }, FileView { label: "Blame", href: blame_url, active: true }])}
                    span { class: "repo-blob-stats", "{stats}" }
                }
                div { class: "repo-blob-actions",
                    {file_downloads(&raw, name, Some((page.content.as_str(), "Copy code")))}
                    {more_menu(&page.path, &page.permalink, delete_link, Some((raw.as_str(), name)))}
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
    };
    chrome(
        repo,
        file_layout(
            repo,
            &page.reference,
            &page.path,
            &page.folders,
            &page.branches,
            &page.tags,
            content,
        ),
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

use crate::webui::pages::repos::{
    chrome::{chrome, icon, RepoChrome},
    pulls::{
        diff::{diff_view, view_toggle},
        Diff,
    },
    ref_path::file_url,
    ref_switcher::{RefOption, RefSwitcher},
};
use crate::webui::{
    components::{compact_date, short_sha, Avatar, CopyButton, LiveTime},
    navigation::NavLink,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Commit {
    pub sha: String,
    pub message: String,
    pub author: String,
    pub author_date: String,
    pub committer: String,
    pub committer_date: String,
    pub parents: Vec<String>,
}

/// A commit date as an age ("3 days ago"), with the exact time as its hover text.
pub(in crate::webui::pages::repos) fn commit_date(value: &str) -> Element {
    rsx! { LiveTime { value: value.to_owned(), title: Some(value.to_owned()) } }
}

/// The commit that last changed a file or folder, with its people, the rest of its message, and a
/// link to the history of `path` on `reference`.
pub(in crate::webui::pages::repos) fn latest_commit_bar(
    base: &str,
    reference: &str,
    path: &str,
    commit: &Commit,
) -> Element {
    let url = format!(
        "{base}/commit/{}",
        crate::webui::encode_component(&commit.sha)
    );
    let people = commit_people(commit);
    let body = commit_body(&commit.message);
    rsx! {
        div { class: "repo-latest-commit bg-muted/40 border-b border-border flex items-center justify-between gap-4 text-sm",
            div { class: "repo-commit-summary min-w-0",
                span { class: "repo-avatar-stack",
                    for name in people.iter() { Avatar { name: name.clone(), size: 20 } }
                }
                span { class: "repo-commit-author",
                    for (index, name) in people.iter().enumerate() {
                        if index > 0 { {joint(index, people.len())} }
                        strong { "{name}" }
                    }
                }
                NavLink { class: "repo-commit-message".to_owned(), href: url.clone(),
                    span { title: commit.message.clone(), "{commit.message.lines().next().unwrap_or(\"\")}" }
                }
                if !body.is_empty() {
                    details { class: "repo-commit-more",
                        summary { title: "Show the rest of the commit message", "aria-label": "Show the rest of the commit message", "…" }
                        pre { class: "repo-commit-body", "{body}" }
                    }
                }
            }
            div { class: "repo-commit-meta",
                NavLink { class: "repo-short-sha".to_owned(), href: url, "{short_sha(&commit.sha)}" }
                {commit_date(&commit.author_date)}
                NavLink { class: "repo-history-link".to_owned(), href: history_url(base, reference, path), {icon("commits")} span { "History" } }
            }
        }
    }
}

/// The history page for `reference`, narrowed to `path` when one is given.
pub(in crate::webui::pages::repos) fn history_url(
    base: &str,
    reference: &str,
    path: &str,
) -> String {
    let url = format!(
        "{base}/commits?ref={}",
        crate::webui::encode_component(reference)
    );
    if path.is_empty() {
        url
    } else {
        format!("{url}&path={}", crate::webui::encode_component(path))
    }
}

/// The commit's author, then the people its `Co-authored-by:` trailers name, in message order.
fn commit_people(commit: &Commit) -> Vec<String> {
    let mut people = vec![commit.author.clone()];
    people.extend(commit.message.lines().filter_map(|line| {
        let (key, value) = line.split_once(':')?;
        if !key.trim().eq_ignore_ascii_case("co-authored-by") {
            return None;
        }
        let name = value.split('<').next()?.trim();
        (!name.is_empty()).then(|| name.to_owned())
    }));
    people
}

/// The message below its subject line, without the blank lines around it.
fn commit_body(message: &str) -> &str {
    message.split_once('\n').map_or("", |(_, body)| body).trim()
}

/// The words between two names in a list: "A and B", "A, B and C".
fn joint(index: usize, count: usize) -> &'static str {
    if index + 1 == count {
        " and "
    } else {
        ", "
    }
}

pub(in crate::webui::pages::repos) fn commit_row(base: &str, commit: &Commit) -> Element {
    let href = format!(
        "{base}/commit/{}",
        crate::webui::encode_component(&commit.sha)
    );
    let browse = file_url(base, "tree", &commit.sha, "");
    rsx! {
        article { class: "repo-commit-row",
            div { class: "repo-commit-node", {icon("commits")} }
            div { class: "repo-commit-row-content",
                NavLink { class: "repo-history-message".to_owned(), href: href.clone(), "{commit.message.lines().next().unwrap_or(\"\")}" }
                p { class: "repo-history-author", Avatar { name: commit.author.clone(), size: 20 } strong { "{commit.author}" } " committed " {commit_date(&commit.committer_date)} }
            }
            div { class: "repo-commit-row-actions",
                NavLink { class: "repo-short-sha".to_owned(), href, "{short_sha(&commit.sha)}" }
                CopyButton { text: commit.sha.clone(), label: "Copy SHA".to_owned(), icon_only: true }
                NavLink { class: "repo-icon-button".to_owned(), href: browse, title: Some("Browse files".to_owned()), aria_label: Some("Browse files".to_owned()), {icon("code")} }
            }
        }
    }
}

/// The history of `reference`, or of `path` on it when one is given. The ref menu keeps the path,
/// so switching branches shows the same file's history.
#[expect(
    clippy::too_many_arguments,
    reason = "The page takes its rows, its ref, path and filters, its paging and both ref lists."
)]
pub fn commits(
    repo: &RepoChrome,
    rows: &[Commit],
    reference: &str,
    path: &str,
    filter: &HistoryFilter,
    page: usize,
    has_more: bool,
    branches: &[String],
    tags: &[String],
) -> Element {
    let base = repo.base();
    // Switching the ref keeps the path and the filters, so the list stays on the same history.
    let option = |name: &String| RefOption {
        name: name.clone(),
        href: format!("{}{}", history_url(&base, name, path), filter_query(filter)),
    };
    let branch_options: Vec<RefOption> = branches.iter().map(&option).collect();
    let tag_options: Vec<RefOption> = tags
        .iter()
        .map(|name| RefOption {
            name: name.clone(),
            href: format!(
                "{}{}",
                history_url(&base, &format!("refs/tags/{name}"), path),
                filter_query(filter)
            ),
        })
        .collect();
    let heading = if path.is_empty() {
        "Commits".to_owned()
    } else {
        format!("Commits to {path}")
    };
    let history = history_url(&base, reference, path);
    let pager = format!("{history}{}", filter_query(filter));
    let days = commits_by_day(rows);
    chrome(
        repo,
        rsx! {
            section { class: "repo-history-page",
                header { class: "repo-page-heading",
                    div {
                        h2 { "{heading}" }
                        p { "History for " code { class: "repo-ref-pill", "{reference}" }
                            if !path.is_empty() { span { " · " NavLink { href: history_url(&base, reference, ""), "Show all commits" } } }
                        }
                    }
                    RefSwitcher { base: base.clone(), current: reference.to_owned(), branches: branch_options, tags: tag_options, default_branch: repo.default_branch.clone() }
                }
                {history_filters(&base, reference, path, filter, rows)}
                if rows.is_empty() {
                    div { class: "repo-empty panel", {icon("commits")} h3 { "No commits on this ref" } p { "Choose another branch, tag, or commit to view its history." } }
                } else {
                    for (day, group) in days.iter() {
                        section { class: "repo-commit-day", key: "{day}",
                            h3 { class: "repo-commit-day-heading", "Commits on {day}" }
                            div { class: "repo-commit-list panel", for commit in group.iter() { {commit_row(&base, commit)} } }
                        }
                    }
                }
                nav { class: "repo-pagination", "aria-label": "Commit history",
                    if page > 1 { NavLink { class: "btn btn-secondary".to_owned(), href: format!("{pager}&page={}", page - 1), "Newer" } }
                    if has_more { NavLink { class: "btn btn-secondary repo-pager-older".to_owned(), href: format!("{pager}&page={}", page + 1), "Older" } }
                }
            }
        },
    )
}

/// The author and date filters of the history page, as the user typed them. An empty field is
/// not applied.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistoryFilter {
    pub author: String,
    pub since: String,
    pub until: String,
}

impl HistoryFilter {
    fn is_set(&self) -> bool {
        !self.author.is_empty() || !self.since.is_empty() || !self.until.is_empty()
    }
}

/// The filter fields that are set, as query text such as `&author=ada&since=2026-01-01`.
fn filter_query(filter: &HistoryFilter) -> String {
    [
        ("author", &filter.author),
        ("since", &filter.since),
        ("until", &filter.until),
    ]
    .into_iter()
    .filter(|(_, value)| !value.is_empty())
    .map(|(key, value)| format!("&{key}={}", crate::webui::encode_component(value)))
    .collect()
}

/// The commits of each calendar day, in the order given, with the day as its heading. The day
/// is the first ten characters of the committer date: `YYYY-MM-DD`, as RFC 3339 and plain dates
/// both start.
fn commits_by_day(rows: &[Commit]) -> Vec<(String, Vec<&Commit>)> {
    let mut days: Vec<(&str, Vec<&Commit>)> = Vec::new();
    for commit in rows {
        let text = commit.committer_date.as_str();
        let day = text.get(..10).unwrap_or(text);
        match days.last_mut() {
            Some((last, group)) if *last == day => group.push(commit),
            _ => days.push((day, vec![commit])),
        }
    }
    days.into_iter()
        .map(|(day, group)| (compact_date(day), group))
        .collect()
}

/// The date ranges the Date menu offers as links: the name the page reads, and its title.
const DATE_PRESETS: [(&str, &str); 4] = [
    ("day", "Since yesterday"),
    ("week", "Past week"),
    ("month", "Past month"),
    ("year", "Past year"),
];

/// The filter menus above the list. The Author menu lists the people on this page and has a field for
/// any other name; the Date menu offers ranges and two days. The form keeps the ref and the path, so
/// the list stays on the same history.
fn history_filters(
    base: &str,
    reference: &str,
    path: &str,
    filter: &HistoryFilter,
    rows: &[Commit],
) -> Element {
    let author = if filter.author.is_empty() {
        "Author".to_owned()
    } else {
        format!("Author: {}", filter.author)
    };
    let dates = match (filter.since.as_str(), filter.until.as_str()) {
        ("", "") => "Date".to_owned(),
        (since, "") => format!("Date: from {since}"),
        ("", until) => format!("Date: to {until}"),
        (since, until) => format!("Date: {since} to {until}"),
    };
    let link = |change: HistoryFilter| {
        format!(
            "{}{}",
            history_url(base, reference, path),
            filter_query(&change)
        )
    };
    let option_class = |chosen: bool| {
        if chosen {
            "repo-filter-option repo-filter-chosen"
        } else {
            "repo-filter-option"
        }
    };
    let current = |chosen: bool| chosen.then(|| "true".to_owned());
    let mut people: Vec<&str> = Vec::new();
    for commit in rows {
        if !people.contains(&commit.author.as_str()) {
            people.push(commit.author.as_str());
        }
    }
    rsx! {
        form { class: "repo-commit-filters", method: "get", action: format!("{base}/commits"), "aria-label": "Filter commits",
            input { r#type: "hidden", name: "ref", value: reference.to_owned() }
            if !path.is_empty() { input { r#type: "hidden", name: "path", value: path.to_owned() } }
            details { class: "native-menu",
                summary { class: "btn btn-secondary btn-sm", "{author}" }
                div { class: "native-menu-panel repo-filter-panel repo-filter-popover",
                    h3 { class: "repo-filter-heading", "Filter by author" }
                    label { class: "repo-field",
                        span { class: "sr-only", "Name or email" }
                        input { r#type: "search", name: "author", value: filter.author.clone(), placeholder: "Name or email" }
                    }
                    button { class: "btn btn-primary btn-sm", r#type: "submit", "Apply" }
                    div { class: "repo-filter-list",
                        NavLink { class: option_class(filter.author.is_empty()), aria_current: current(filter.author.is_empty()), href: link(HistoryFilter { author: String::new(), ..filter.clone() }), "All users" }
                        for name in people {
                            NavLink { class: option_class(filter.author == name), aria_current: current(filter.author == name), href: link(HistoryFilter { author: name.to_owned(), ..filter.clone() }),
                                Avatar { name: name.to_owned(), size: 16 }
                                span { "{name}" }
                            }
                        }
                    }
                }
            }
            details { class: "native-menu",
                summary { class: "btn btn-secondary btn-sm", "{dates}" }
                div { class: "native-menu-panel repo-filter-panel repo-filter-popover",
                    h3 { class: "repo-filter-heading", "Filter by date" }
                    div { class: "repo-filter-list",
                        NavLink { class: option_class(filter.since.is_empty() && filter.until.is_empty()), aria_current: current(filter.since.is_empty() && filter.until.is_empty()), href: link(HistoryFilter { since: String::new(), until: String::new(), ..filter.clone() }), "All time" }
                        for (value, title) in DATE_PRESETS {
                            NavLink { class: "repo-filter-option".to_owned(), href: link(HistoryFilter { since: value.to_owned(), until: String::new(), ..filter.clone() }), "{title}" }
                        }
                    }
                    label { class: "repo-field", span { "From" } input { r#type: "date", name: "since", value: filter.since.clone() } }
                    label { class: "repo-field", span { "To" } input { r#type: "date", name: "until", value: filter.until.clone() } }
                    button { class: "btn btn-primary btn-sm", r#type: "submit", "Apply" }
                }
            }
            if filter.is_set() {
                NavLink { class: "repo-filter-clear".to_owned(), href: history_url(base, reference, path), title: Some("Clear filters".to_owned()), aria_label: Some("Clear filters".to_owned()), "×" }
            }
        }
    }
}

/// The commit with its parents, its full ID and the branches and tags that point at it, then the
/// files it changed.
pub fn commit_detail(
    repo: &RepoChrome,
    commit: &Commit,
    diff: &Diff,
    branches: &[String],
    tags: &[String],
    split: bool,
) -> Element {
    let base = repo.base();
    let browse = file_url(&base, "tree", &commit.sha, "");
    let body = commit_body(&commit.message);
    chrome(
        repo,
        rsx! {
            article { class: "repo-commit-detail panel",
                header { class: "repo-commit-detail-header",
                    h2 { "{commit.message.lines().next().unwrap_or(\"Commit\")}" }
                    if !body.is_empty() { pre { "{body}" } }
                }
                div { class: "repo-commit-detail-meta",
                    div { {icon("commits")} p { strong { "{commit.author}" } " authored and " strong { "{commit.committer}" } " committed " {commit_date(&commit.committer_date)} } }
                    div { class: "repo-commit-ids",
                        if !commit.parents.is_empty() {
                            span { class: "repo-commit-parents", "Parents " for parent in &commit.parents { NavLink { class: "repo-short-sha".to_owned(), href: format!("{base}/commit/{}", crate::webui::encode_component(parent)), "{short_sha(parent)}" } } }
                        }
                        div { class: "repo-full-sha-row",
                            code { class: "repo-full-sha", "{commit.sha}" }
                            CopyButton { text: commit.sha.clone(), label: "Copy SHA".to_owned(), icon_only: true }
                        }
                    }
                    if !branches.is_empty() || !tags.is_empty() {
                        div { class: "repo-commit-refs", "aria-label": "Branches and tags at this commit",
                            for name in branches.iter() { NavLink { class: "repo-ref-pill".to_owned(), href: file_url(&base, "tree", name, ""), {icon("branches")} "{name}" } }
                            for name in tags.iter() { NavLink { class: "repo-ref-pill".to_owned(), href: file_url(&base, "tree", &format!("refs/tags/{name}"), ""), {icon("tags")} "{name}" } }
                        }
                    }
                }
                footer { class: "repo-commit-detail-footer",
                    NavLink { class: "btn btn-secondary".to_owned(), href: browse, "Browse files" }
                }
            }
            if !diff.files.is_empty() {
                div { class: "collaboration-actions", {view_toggle(&format!("{base}/commit/{}", commit.sha), split)} }
            }
            {diff_view(diff, split)}
        },
    )
}

use crate::webui::pages::repos::{
    chrome::{chrome, csrf, icon, RepoChrome},
    commits::commit_date,
    pulls::PullSummary,
    ref_path::{compare_url, file_url},
};
use crate::webui::{
    components::{compact_date, confirm_dialog, count_label, DraftInput, DraftTextarea},
    navigation::NavLink,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RefRow {
    pub name: String,
    pub sha: String,
    pub message: Option<String>,
    pub is_default: bool,
    /// How far a branch is ahead of and behind the default branch. `None` for the default branch,
    /// for tags, and for branches past the count limit.
    pub ahead_behind: Option<(usize, usize)>,
    /// When a branch's tip commit or a tag was made, as RFC 3339.
    pub updated: Option<String>,
    /// The open pull request whose head is this branch, when there is one.
    pub pull: Option<PullSummary>,
}

/// The branch or tag list. `query` keeps the rows whose name contains it, ignoring case.
pub fn refs(
    repo: &RepoChrome,
    rows: &[RefRow],
    tags: bool,
    branches: &[RefRow],
    query: &str,
    tab: &str,
    stale: usize,
) -> Element {
    let base = repo.base();
    let kind = if tags { "tag" } else { "branch" };
    let plural = if tags { "tags" } else { "branches" };
    let title = if tags { "Tags" } else { "Branches" };
    let list_url = format!("{base}/{}", if tags { "tags" } else { "branches" });
    // Each other branch links to its comparison with the default branch.
    let default_branch = branches
        .iter()
        .find(|branch| branch.is_default)
        .map(|branch| branch.name.clone())
        .filter(|_| !tags);
    let needle = query.trim().to_lowercase();
    let shown: Vec<&RefRow> = rows
        .iter()
        .filter(|row| row.name.to_lowercase().contains(&needle))
        .collect();
    chrome(
        repo,
        rsx! {
            section { class: "repo-refs-page",
                header { class: "repo-page-heading",
                    div {
                        div { class: "title-with-count", h2 { "{title}" } span { class: "count-badge", {count_label(rows.len(), kind, plural)} } }
                        p { if tags { "Mark important points in this repository's history." } else { "View and manage lines of development." } }
                    }
                }
                if tags {
                    {super::super::releases::releases_tags_nav(repo, false)}
                }
                if !tags {
                    nav { class: "repo-ref-tabs", aria_label: "Branch filter",
                        for (name, label, count) in [("all", "All", branches.len()), ("active", "Active", branches.len() - stale), ("stale", "Stale", stale)] {
                            NavLink { href: if name == "all" { format!("{base}/branches") } else { format!("{base}/branches?tab={name}") }, class: if tab == name || (name == "all" && tab.is_empty()) { "repo-ref-tab is-active" } else { "repo-ref-tab" },
                                "{label}" span { class: "count-badge", "{count}" }
                            }
                        }
                    }
                    form { class: "repo-ref-search", method: "get", action: format!("{base}/branches"), role: "search", "aria-label": "Search branches",
                        if !tab.is_empty() && tab != "all" { input { r#type: "hidden", name: "tab", value: tab.to_owned() } }
                        input { r#type: "search", name: "q", value: query.to_owned(), placeholder: "Find a branch", "aria-label": "Find a branch" }
                        button { class: "repo-icon-button", r#type: "submit", title: "Search branches", "aria-label": "Search branches", {icon("search")} }
                        if !query.is_empty() { NavLink { class: "btn btn-secondary btn-sm".to_owned(), href: format!("{base}/branches"), "Clear" } }
                    }
                }
                div { class: if repo.can_write { "repo-refs-layout" } else { "repo-refs-layout repo-refs-layout-full" },
                    section { class: "repo-ref-list-panel panel", aria_label: title,
                        header { class: "repo-list-header",
                            strong { "{title}" }
                        }
                        if rows.is_empty() && (tab.is_empty() || tab == "all") {
                            div { class: "repo-empty",
                                {icon(if tags { "tags" } else { "branches" })}
                                h3 { "No {title.to_lowercase()} yet" }
                                p { if tags { "Create a tag to mark a release or milestone." } else { "Create a branch from an existing commit." } }
                            }
                        } else if shown.is_empty() {
                            div { class: "repo-empty",
                                {icon("search")}
                                h3 { if query.is_empty() { "No {tab} {plural}" } else { "No matches" } }
                                p { if query.is_empty() { "Nothing to show here." } else { "No {plural} match \"{query}\"." } }
                            }
                        }
                        for row in &shown {
                            article { key: "{row.name}", class: "repo-ref-row",
                                div { class: "repo-ref-row-icon", {icon(if tags { "tags" } else { "branches" })} }
                                div { class: "repo-ref-row-content",
                                    div { class: "repo-ref-row-title",
                                        NavLink { href: file_url(&base, "tree", &format!("refs/{}/{}", if tags { "tags" } else { "heads" }, row.name), ""), "{row.name}" }
                                        if row.is_default { span { class: "repo-label-neutral", "Default" } }
                                        if let Some(pull) = &row.pull {
                                            NavLink { class: "repo-ref-pull", href: format!("{base}/pulls/{}", pull.number), title: Some(pull.title.clone()), {icon("pulls")} "#{pull.number}" }
                                        }
                                    }
                                    if let Some(message) = &row.message { p { "{message}" } }
                                    if !row.sha.is_empty() {
                                        p { class: "repo-ref-meta",
                                            if tags { "Tagged " }
                                            NavLink { class: "repo-short-sha".to_owned(), href: format!("{base}/commit/{}", crate::webui::encode_component(&row.sha)), "{row.sha.get(..7).unwrap_or(&row.sha)}" }
                                            if let Some(updated) = row.updated.as_ref().filter(|_| !tags) { " updated " {commit_date(updated)} }
                                        }
                                    }
                                    if let (Some((ahead, behind)), Some(default)) = (row.ahead_behind, default_branch.as_deref()) {
                                        p { class: "repo-ref-meta", "{ahead} ahead, {behind} behind {default}" }
                                    }
                                }
                                div { class: "repo-ref-actions",
                                    if let Some(updated) = row.updated.as_ref().filter(|_| tags) {
                                        span { class: "repo-ref-date", {ref_date(updated)} }
                                    }
                                    if tags {
                                        a { class: "repo-icon-button", href: format!("{base}/archive/{}.zip", crate::webui::encode_component(&format!("refs/tags/{}", row.name))), title: "Download ZIP", "aria-label": "Download {row.name} as ZIP", {icon("download")} }
                                    }
                                    if let Some(default) = default_branch.as_deref().filter(|_| !row.is_default) {
                                        NavLink { class: "btn btn-secondary btn-sm", href: compare_url(&base, default, &if tags { format!("refs/tags/{}", row.name) } else { row.name.clone() }), "Compare" }
                                    }
                                    if repo.can_write && !row.is_default {
                                        details { class: "native-menu repo-delete-menu",
                                            summary { class: "repo-icon-button repo-icon-danger", title: "Delete {kind}", "aria-label": "Delete {kind}", {icon("trash")} }
                                            // A centred dialog over the page, so opening it moves nothing. Cancel and
                                            // the backdrop reload the list, which closes it without JavaScript.
                                            {confirm_dialog(None, &format!("Delete {kind} {}", row.name), &list_url, &repo.action(&format!("delete-{kind}")), rsx! {
                                                {csrf(&repo.csrf_token)}
                                                input { r#type: "hidden", name: "repository_id", value: "{repo.id}" }
                                                input { r#type: "hidden", name: "name", value: row.name.clone() }
                                                strong { "Delete {kind} {row.name}?" }
                                                p { "This cannot be undone." }
                                                label { class: "repo-check-label", input { r#type: "checkbox", required: true, name: "confirm", value: "yes" } " I understand" }
                                            }, &format!("Delete {kind}"))}
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if repo.can_write {
                        aside { class: "repo-ref-create", aria_label: format!("Create {kind}"),
                            CreateRefForm { key: "{base}:{kind}", repo: repo.clone(), tags, branches: branches.to_vec() }
                        }
                    }
                }
            }
        },
    )
}

/// A tag's date as the calendar day it was made, with the exact time as its hover text.
fn ref_date(value: &str) -> Element {
    rsx! { time { datetime: value.to_owned(), title: value.to_owned(), "{compact_date(value)}" } }
}

#[component]
fn CreateRefForm(repo: RepoChrome, tags: bool, branches: Vec<RefRow>) -> Element {
    let kind = if tags { "tag" } else { "branch" };
    // The form posts the full commit SHA of the chosen branch. The default branch comes first
    // and starts selected.
    rsx! {
        form { method: "post", action: repo.action(&format!("create-{kind}")), class: "repo-settings-section panel",
            {csrf(&repo.csrf_token)}
            input { r#type: "hidden", name: "repository_id", value: "{repo.id}" }
            header { class: "repo-section-heading",
                div { h3 { "Create a {kind}" } p { "Start from an existing branch." } }
            }
            div { class: "repo-form-body",
                label { class: "repo-field", span { "Name" } DraftInput { name: "name", required: true, maxlength: Some(255), initial_value: String::new(), placeholder: format!("New {kind} name") } }
                label { class: "repo-field", span { "Start from branch" }
                    // Native selection tracks the option, not its possibly shared SHA.
                    select { name: "sha", required: true,
                        for (index, branch) in branches.iter().enumerate() {
                            option { key: "{branch.name}", value: branch.sha.clone(), "selected": index == 0, "{branch.name} ({branch.sha.get(..7).unwrap_or(&branch.sha)})" }
                        }
                    }
                }
                if tags { label { class: "repo-field", span { "Message " small { "Optional" } } DraftTextarea { name: "message", rows: 3, initial_value: String::new() } } }
                button { class: "btn btn-primary", r#type: "submit", "Create {kind}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn branch_names_are_distinct_options_even_when_their_commits_match() {
        let branch = RefRow {
            name: "main".into(),
            sha: "a".repeat(40),
            message: None,
            is_default: true,
            ahead_behind: None,
            updated: None,
            pull: None,
        };
        let topic = RefRow {
            name: "-topic".into(),
            is_default: false,
            ..branch.clone()
        };
        let repo = RepoChrome {
            owner: "u".into(),
            name: "r".into(),
            can_write: true,
            ..Default::default()
        };
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(refs(
                &repo,
                &[branch.clone(), topic.clone()],
                false,
                &[branch, topic],
                "",
                "",
                0,
            ))
        });
        assert!(html.contains("href=\"/u/r/tree/refs/heads/-topic\""));
        let options: Vec<_> = html.split("<option").skip(1).collect();
        assert_eq!(options.len(), 2);
        assert!(options[0].contains("main (aaaaaaa)"));
        assert!(options[1].contains("-topic (aaaaaaa)"));
        assert!(options[0].split('>').next().unwrap().contains("selected"));
        assert!(!options[1].split('>').next().unwrap().contains("selected"));
        for option in options {
            assert!(option.contains(&format!("value=\"{}\"", "a".repeat(40))));
        }
    }

    #[test]
    fn a_tag_row_browses_its_object_not_a_same_named_branch() {
        let row = RefRow {
            name: "main".into(),
            sha: "a".repeat(40),
            message: None,
            is_default: false,
            ahead_behind: None,
            updated: None,
            pull: None,
        };
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(refs(
                &RepoChrome {
                    owner: "u".into(),
                    name: "r".into(),
                    ..Default::default()
                },
                &[row],
                true,
                &[],
                "",
                "",
                0,
            ))
        });
        assert!(html.contains("href=\"/u/r/tree/refs/tags/main\""));
        assert!(!html.contains("href=\"/u/r/tree/main\""));
    }
}

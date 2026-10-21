//! The signed-in user's issue and pull request lists, across the repositories they can read.
//! The rows use the repository list classes, so the lists look the same in both places.
use crate::webui::{
    encode_component,
    navigation::NavLink,
    pages::repos::issues::{label_style, Kind, Label},
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    pub owner: String,
    pub repo: String,
    pub number: i32,
    pub title: String,
    /// "open" or "closed"; a pull request can also be "merged".
    pub state: String,
    /// Issues only. Pull requests in this list have no labels.
    pub labels: Vec<Label>,
    /// Issues only: the comment count. A pull request has none.
    pub comments: Option<i64>,
    /// The creation time, for the `datetime` of the time element.
    pub created: String,
    /// The creation time as text, such as "3 days ago".
    pub opened: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkList {
    pub kind: Kind,
    /// The selected tab: "assigned", "created", "involved" or "review-requested".
    pub tab: String,
    /// Whether the open or the closed items are listed.
    pub open: bool,
    /// Counts for the selected tab, in both states.
    pub open_count: i64,
    pub closed_count: i64,
    /// True when the list stopped at the limit, so older matches are not shown.
    pub limited: bool,
    pub items: Vec<WorkItem>,
}

/// The tabs of a list in order: the value the address carries, and its label.
fn tabs(kind: Kind) -> &'static [(&'static str, &'static str)] {
    match kind {
        Kind::Issue => &[("assigned", "Assigned to me"), ("created", "Created by me")],
        Kind::Pull => &[
            ("created", "Created by me"),
            ("assigned", "Assigned to me"),
            ("review-requested", "Review requests"),
            ("involved", "Involving me"),
        ],
    }
}

fn list_href(kind: Kind, tab: &str, open: bool) -> String {
    let state = if open { "open" } else { "closed" };
    format!("/{}?tab={tab}&state={state}", kind.family())
}

/// The open, merged or closed icon. Its paths match the repository list rows.
fn state_icon(kind: Kind, state: &str) -> Element {
    let class = match state {
        "open" => "collaboration-item-icon collaboration-icon-open",
        "merged" => "collaboration-item-icon collaboration-icon-merged",
        _ => "collaboration-item-icon collaboration-icon-closed",
    };
    rsx! {
        svg { class, view_box: "0 0 16 16", width: "16", height: "16", fill: "none", stroke: "currentColor", stroke_width: "1.5", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true", "focusable": "false",
            if kind == Kind::Issue {
                circle { cx: "8", cy: "8", r: "6.25" }
                if state == "open" { circle { cx: "8", cy: "8", r: ".75", fill: "currentColor", stroke: "none" } }
                else { path { d: "m5 8 2 2 4-4" } }
            } else {
                circle { cx: "4", cy: "3", r: "1.75" }
                circle { cx: "12", cy: "13", r: "1.75" }
                path { d: "M4 5v9" }
                if state == "merged" { path { d: "M12 11V8c0-3-8-1-8-5" } }
                else { path { d: "M12 11V5c0-1.1-.9-2-2-2H8m2-2L8 3l2 2" } }
            }
        }
    }
}

fn comment_icon() -> Element {
    rsx! {
        svg { view_box: "0 0 16 16", width: "14", height: "14", fill: "none", stroke: "currentColor", stroke_width: "1.5", stroke_linejoin: "round", "aria-hidden": "true", "focusable": "false",
            path { d: "M2 3.5A1.5 1.5 0 0 1 3.5 2h9A1.5 1.5 0 0 1 14 3.5v6A1.5 1.5 0 0 1 12.5 11H7l-3 3v-3H3.5A1.5 1.5 0 0 1 2 9.5z" }
        }
    }
}

fn work_row(kind: Kind, item: &WorkItem) -> Element {
    let repo_href = format!(
        "/{}/{}",
        encode_component(&item.owner),
        encode_component(&item.repo)
    );
    let item_href = format!("{repo_href}/{}/{}", kind.family(), item.number);
    let repo_name = format!("{}/{}", item.owner, item.repo);
    rsx! {
        article { class: "collaboration-row",
            div { class: "collaboration-row-icon", title: item.state.clone(), {state_icon(kind, &item.state)} span { class: "sr-only", "{item.state} {kind.singular()}" } }
            div { class: "collaboration-row-content",
                div { class: "collaboration-row-title",
                    NavLink { class: "collaboration-title-link", href: item_href, "{item.title}" }
                    for label in &item.labels { span { class: "collaboration-label", style: label_style(&label.color), "{label.name}" } }
                }
                p { class: "collaboration-row-meta",
                    NavLink { href: repo_href, "{repo_name}" }
                    " #{item.number} opened "
                    time { datetime: item.created.clone(), "{item.opened}" }
                }
            }
            if let Some(comments) = item.comments {
                span { class: "collaboration-row-side", title: "Comments", aria_label: format!("{comments} comments"), {comment_icon()} "{comments}" }
            }
        }
    }
}

fn empty_title(kind: Kind, tab: &str, open: bool) -> String {
    let state = if open { "open" } else { "closed" };
    let noun = match kind {
        Kind::Issue => "issues",
        Kind::Pull => "pull requests",
    };
    let scope = match tab {
        "assigned" => " assigned to you",
        "review-requested" => " waiting for your review",
        "involved" => " involving you",
        _ => "",
    };
    format!("No {state} {noun}{scope}")
}

pub fn render(list: WorkList) -> Element {
    let kind = list.kind;
    let (heading, plural) = match kind {
        Kind::Issue => ("Issues", "issues"),
        Kind::Pull => ("Pull requests", "pull requests"),
    };
    let state = if list.open { "open" } else { "closed" };
    rsx! {
        div { class: "repo-content",
            section { class: "collaboration-page collaboration-list", aria_label: heading,
                div { class: "collaboration-toolbar",
                    div { class: "collaboration-title-block",
                        h2 { "{heading}" }
                        p { "Across the repositories you can read." }
                    }
                }
                nav { class: "collaboration-state-filters", aria_label: "Lists",
                    for (value, title) in tabs(kind).iter().copied() {
                        NavLink { href: list_href(kind, value, list.open), mark_current: false,
                            class: if value == list.tab { "collaboration-filter-link collaboration-filter-active" } else { "collaboration-filter-link" },
                            aria_current: if value == list.tab { Some("page".to_owned()) } else { None },
                            // A phone shows the first word, so the four tabs stay on one row.
                            {
                                let (first, rest) = title.split_once(' ').unwrap_or((title, ""));
                                rsx! { "{first}" if !rest.is_empty() { span { class: "collaboration-filter-rest", " {rest}" } } }
                            }
                        }
                    }
                }
                div { class: "collaboration-panel collaboration-list-panel",
                    header { class: "collaboration-panel-header collaboration-list-header",
                        nav { class: "collaboration-state-filters", aria_label: "Entry state",
                            for (open, count) in [(true, list.open_count), (false, list.closed_count)] {
                                NavLink { href: list_href(kind, &list.tab, open), mark_current: false,
                                    class: if open == list.open { "collaboration-filter-link collaboration-filter-active" } else { "collaboration-filter-link" },
                                    aria_current: if open == list.open { Some("page".to_owned()) } else { None },
                                    {state_icon(kind, if open { "open" } else { "closed" })}
                                    span { class: "collaboration-filter-count", "{count}" }
                                    span { if open { "Open" } else { "Closed" } }
                                }
                            }
                        }
                    }
                    if list.items.is_empty() {
                        div { class: "collaboration-empty",
                            {state_icon(kind, state)}
                            h3 { "{empty_title(kind, &list.tab, list.open)}" }
                        }
                    }
                    for item in &list.items {
                        {work_row(kind, item)}
                    }
                }
                if list.limited { p { class: "collaboration-limit collaboration-muted", "Showing the latest 100 matching {plural}." } }
            }
        }
    }
}

/// A repository the signed-in user can open an issue in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepositoryChoice {
    pub owner: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RepositoryChoices {
    pub repositories: Vec<RepositoryChoice>,
    pub page: u32,
    pub query: String,
    pub has_older: bool,
}

/// `/issues/new`: the repositories to open an issue in. Each one links to its new issue form.
pub fn choose_repository(model: &RepositoryChoices) -> Element {
    let choices = &model.repositories;
    let href = |page| {
        format!(
            "/issues/new?page={page}&q={}",
            encode_component(&model.query)
        )
    };
    rsx! {
        div { class: "repo-content",
            section { class: "collaboration-page collaboration-list", aria_label: "New issue",
                div { class: "collaboration-toolbar",
                    div { class: "collaboration-title-block",
                        h2 { "New issue" }
                        p { "Choose the repository for the issue." }
                    }
                    form { method: "get", action: "/issues/new",
                        input { name: "q", value: model.query.clone(), maxlength: 256, aria_label: "Find a repository" }
                        button { r#type: "submit", class: "btn", "Search" }
                    }
                }
                div { class: "collaboration-panel collaboration-list-panel",
                    if choices.is_empty() {
                        div { class: "collaboration-empty",
                            h3 { "No repositories yet" }
                            p { "Create a repository, then open an issue in it." }
                            NavLink { class: "btn btn-primary", href: "/new", "New repository" }
                        }
                    }
                    for choice in choices {
                        article { class: "collaboration-row",
                            div { class: "collaboration-row-content",
                                div { class: "collaboration-row-title",
                                    NavLink { class: "collaboration-title-link", href: format!("/{}/{}/issues/new", encode_component(&choice.owner), encode_component(&choice.name)), "{choice.owner}/{choice.name}" }
                                }
                            }
                        }
                    }
                }
                nav { aria_label: "Repository pages",
                    if model.page > 1 { NavLink { href: href(model.page-1), class: "btn", "Previous page" } }
                    if model.has_older { NavLink { href: href(model.page+1), class: "btn", "Next page" } }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn work_list_links_rows_to_their_repository_and_keeps_tab_and_state() {
        let list = WorkList {
            kind: Kind::Issue,
            tab: "created".into(),
            open: false,
            open_count: 2,
            closed_count: 1,
            limited: false,
            items: vec![WorkItem {
                owner: "alice".into(),
                repo: "project".into(),
                number: 7,
                title: "<b>Broken</b>".into(),
                state: "closed".into(),
                labels: vec![],
                comments: Some(3),
                created: "2026-10-01T00:00:00Z".into(),
                opened: "2 weeks ago".into(),
            }],
        };
        let html = dioxus_ssr::render_element(render(list));
        assert!(html.contains("href=\"/alice/project/issues/7\""), "{html}");
        assert!(html.contains("&#60;b&#62;Broken"), "{html}");
        assert!(html.contains("href=\"/alice/project\""), "{html}");
        assert!(html.contains("2 weeks ago"), "{html}");
        assert!(
            html.contains("Created") && html.contains(" by me"),
            "{html}"
        );
        assert!(html.contains("aria-current=\"page\""), "{html}");
        assert!(html.contains("tab=created"), "{html}");
        assert!(html.contains("state=open"), "{html}");
        assert!(!html.contains("class=\"collaboration-empty\""), "{html}");
        assert!(!html.contains("Showing the latest"), "{html}");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn empty_pull_request_review_list_says_what_is_missing() {
        let list = WorkList {
            kind: Kind::Pull,
            tab: "review-requested".into(),
            open: true,
            open_count: 0,
            closed_count: 0,
            limited: false,
            items: vec![],
        };
        let html = dioxus_ssr::render_element(render(list));
        assert!(
            html.contains("No open pull requests waiting for your review"),
            "{html}"
        );
        assert!(
            html.contains("Review") && html.contains(" requests"),
            "{html}"
        );
        assert!(html.contains("state=closed"), "{html}");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn involving_tab_is_listed_and_its_empty_state_names_the_scope() {
        let list = WorkList {
            kind: Kind::Pull,
            tab: "involved".into(),
            open: true,
            open_count: 0,
            closed_count: 0,
            limited: false,
            items: vec![],
        };
        let html = dioxus_ssr::render_element(render(list));
        assert!(html.contains("Involving") && html.contains(" me"), "{html}");
        assert!(html.contains("tab=involved"), "{html}");
        assert!(
            html.contains("No open pull requests involving you"),
            "{html}"
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn new_issue_chooser_links_each_repository_to_its_issue_form() {
        let choices = vec![RepositoryChoice {
            owner: "alice".into(),
            name: "project".into(),
        }];
        let html = dioxus_ssr::render_element(choose_repository(&RepositoryChoices {
            repositories: choices,
            page: 1,
            ..Default::default()
        }));
        assert!(
            html.contains("href=\"/alice/project/issues/new\""),
            "{html}"
        );
        assert!(!html.contains("No repositories yet"), "{html}");
        let empty = dioxus_ssr::render_element(choose_repository(&RepositoryChoices::default()));
        assert!(empty.contains("No repositories yet"), "{empty}");
        assert!(empty.contains("href=\"/new\""), "{empty}");
    }
}

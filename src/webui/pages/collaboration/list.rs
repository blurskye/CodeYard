use super::{
    detail::{author_link, item_icon, labels},
    Item, Kind, ListCounts, ListFilters, Repository,
};
use crate::webui::{
    components::{compact_date, Avatar, DraftSelect, SelectOption},
    encode_component,
    navigation::NavLink,
    pages::repositories,
};
use dioxus::prelude::*;

pub(super) fn empty_title(kind: Kind, state: &str, query: &str) -> String {
    let plural = match kind {
        Kind::Issue => "issues",
        Kind::Pull => "pull requests",
    };
    if !query.is_empty() {
        return match state {
            "open" => format!("No open {plural} match \"{query}\""),
            "closed" => format!("No closed {plural} match \"{query}\""),
            _ => format!("No {plural} match \"{query}\""),
        };
    }
    match state {
        "open" => format!("No open {plural}"),
        "closed" => format!("No closed {plural}"),
        _ => format!("No {plural} yet"),
    }
}
fn contribute(repo: &Repository, kind: Kind) -> Element {
    rsx! {
        if repo.signed_in {
            NavLink { class: "btn btn-primary", href: format!("{}/new", repo.family_url(kind)), "New {kind.singular()}" }
        } else {
            NavLink { class: "btn btn-secondary", href: "/login", "Sign in to contribute" }
        }
    }
}

/// A list link that keeps the state, the search text and every filter that is set, so moving
/// between tabs or clearing the search does not drop the rest. Empty and default values are left
/// out of the address.
fn list_href(url: &str, state: &str, query: &str, filters: &ListFilters) -> String {
    let mut params = vec![("state", state), ("q", query)];
    params.extend(
        filters
            .selected_labels
            .iter()
            .map(|name| ("label", name.as_str())),
    );
    params.extend([
        ("assignee", filters.assignee.as_str()),
        ("milestone", filters.milestone.as_str()),
        ("author", filters.author.as_str()),
    ]);
    if !filters.sort.is_empty() && filters.sort != "newest" {
        params.push(("sort", filters.sort.as_str()));
    }
    let query_string = params
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(name, value)| format!("{name}={}", encode_component(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{url}?{query_string}")
}

/// The list rows: one per issue or pull request, with its state, title, labels and author. On the
/// right are its milestone and assignees. Milestone pages show the same rows.
pub(super) fn rows(url: &str, kind: Kind, items: &[Item]) -> Element {
    rsx! {
        for item in items {
            article { class: "collaboration-row",
                div { class: "collaboration-row-icon", title: item.state.clone(), {item_icon(kind, &item.state)} span { class: "repo-sr-only", "{item.state} {kind.singular()}" } }
                div { class: "collaboration-row-content",
                    div { class: "collaboration-row-title",
                        NavLink { class: "collaboration-title-link", href: format!("{url}/{}", item.number), "{item.title}" }
                        {labels(&item.labels)}
                        if item.comparison.as_ref().is_some_and(|comparison| comparison.draft) { span { class: "collaboration-muted", "Draft" } }
                        if item.stacked { span { class: "collaboration-muted", "Stack" } }
                    }
                    p { class: "collaboration-row-meta", "#{item.number} opened on " time { datetime: item.created.clone(), "{compact_date(&item.created)}" } " by " {author_link(&item.author)} }
                    if let Some(compare) = &item.comparison { p { class: "collaboration-muted", "{compare.head_label} → {compare.base}" } }
                }
                if item.milestone.is_some() || !item.assignees.is_empty() {
                    div { class: "collaboration-row-side",
                        if let Some(milestone) = &item.milestone { span { class: "collaboration-row-milestone", "{milestone.title}" } }
                        for name in &item.assignees {
                            span { class: "collaboration-row-assignee", title: name.clone(), role: "img", aria_label: name.clone(), Avatar { name: name.clone(), size: 20 } }
                        }
                    }
                }
            }
        }
    }
}

/// The toolbar under the state tabs: the search field, then the filters. Each control shows its
/// purpose inside the box (`Label: Any`). It is a plain GET form, so it works without JavaScript.
fn filter_bar(url: &str, kind: Kind, state: &str, query: &str, filters: &ListFilters) -> Element {
    const SORTS: [(&str, &str); 4] = [
        ("newest", "Newest"),
        ("oldest", "Oldest"),
        ("recently-updated", "Recently updated"),
        ("most-commented", "Most commented"),
    ];
    let sort = if filters.sort.is_empty() {
        "newest"
    } else {
        filters.sort.as_str()
    };
    let plural = kind.title().to_lowercase();
    rsx! {
        form { class: "collaboration-filters filter-toolbar", method: "get", action: url.to_owned(), role: "search", aria_label: "Filter {plural}",
            input { r#type: "hidden", name: "state", value: state.to_owned() }
            // The chosen labels travel with the form, so picking another label adds to them.
            for name in &filters.selected_labels {
                input { r#type: "hidden", name: "label", value: name.clone() }
            }
            input { r#type: "search", name: "q", class: "collaboration-input filter-search", value: query.to_owned(), maxlength: "200", placeholder: "Search {plural}", aria_label: "Search {plural}" }
            label { class: "filter-control",
                span { "Label:" }
                DraftSelect { name: "label", initial_value: String::new(),
                    options: std::iter::once(SelectOption::new("", "Any"))
                        .chain(filters.labels.iter().map(|name| SelectOption::new(name.clone(), name.clone())))
                        .collect::<Vec<_>>() }
            }
            label { class: "filter-control",
                span { "Assignee:" }
                DraftSelect { name: "assignee", initial_value: filters.assignee.clone(),
                    options: vec![SelectOption::new("", "Anyone"), SelectOption::new("none", "No one")]
                        .into_iter()
                        .chain(filters.assignees.iter().map(|name| SelectOption::new(name.clone(), name.clone())))
                        .collect::<Vec<_>>() }
            }
            label { class: "filter-control",
                span { "Milestone:" }
                DraftSelect { name: "milestone", initial_value: filters.milestone.clone(),
                    options: vec![SelectOption::new("", "Any"), SelectOption::new("none", "No milestone")]
                        .into_iter()
                        .chain(filters.milestones.iter().map(|milestone| SelectOption::new(milestone.number.to_string(), milestone.title.clone())))
                        .collect::<Vec<_>>() }
            }
            label { class: "filter-control",
                span { "Author:" }
                DraftSelect { name: "author", initial_value: filters.author.clone(),
                    options: std::iter::once(SelectOption::new("", "Anyone"))
                        .chain(filters.authors.iter().map(|name| SelectOption::new(name.clone(), name.clone())))
                        .collect::<Vec<_>>() }
            }
            label { class: "filter-control",
                span { "Sort:" }
                DraftSelect { name: "sort", initial_value: sort.to_owned(),
                    options: SORTS.iter().map(|(value, title)| SelectOption::new(*value, *title)).collect::<Vec<_>>() }
            }
            button { r#type: "submit", class: "btn btn-sm", "Filter" }
        }
    }
}

/// The active filters as (chip text, the filters without that one), in toolbar order. Sort is
/// not a filter, so it has no chip.
fn active_filters(filters: &ListFilters) -> Vec<(String, ListFilters)> {
    let mut chips = Vec::new();
    for name in &filters.selected_labels {
        let without = ListFilters {
            selected_labels: filters
                .selected_labels
                .iter()
                .filter(|other| *other != name)
                .cloned()
                .collect(),
            ..filters.clone()
        };
        chips.push((format!("Label: {name}"), without));
    }
    if !filters.assignee.is_empty() {
        let text = if filters.assignee == "none" {
            "Assignee: No one".to_owned()
        } else {
            format!("Assignee: {}", filters.assignee)
        };
        chips.push((
            text,
            ListFilters {
                assignee: String::new(),
                ..filters.clone()
            },
        ));
    }
    if !filters.milestone.is_empty() {
        let text = match filters.milestone.as_str() {
            "none" => "Milestone: No milestone".to_owned(),
            number => {
                let title = filters
                    .milestones
                    .iter()
                    .find(|milestone| milestone.number.to_string() == number)
                    .map_or(number, |milestone| milestone.title.as_str());
                format!("Milestone: {title}")
            }
        };
        chips.push((
            text,
            ListFilters {
                milestone: String::new(),
                ..filters.clone()
            },
        ));
    }
    if !filters.author.is_empty() {
        let text = format!("Author: {}", filters.author);
        chips.push((
            text,
            ListFilters {
                author: String::new(),
                ..filters.clone()
            },
        ));
    }
    chips
}

/// The active filters as removable chips under the toolbar, each linking to the list without
/// that filter, and a link that clears them all. Renders nothing when no filter is set.
fn filter_chips(url: &str, state: &str, query: &str, filters: &ListFilters) -> Element {
    let chips = active_filters(filters);
    if chips.is_empty() {
        return rsx! {};
    }
    let cleared = ListFilters {
        selected_labels: Vec::new(),
        assignee: String::new(),
        milestone: String::new(),
        author: String::new(),
        ..filters.clone()
    };
    rsx! {
        div { class: "collaboration-filter-chips", aria_label: "Active filters",
            for (text, without) in chips {
                NavLink { class: "collaboration-filter-chip", href: list_href(url, state, query, &without),
                    span { "{text}" }
                    span { class: "collaboration-filter-chip-remove", "aria-hidden": "true", "×" }
                    span { class: "repo-sr-only", "Remove filter" }
                }
            }
            NavLink { class: "collaboration-filter-clear", href: list_href(url, state, query, &cleared), "Clear all filters" }
        }
    }
}

pub fn list(
    repo: &Repository,
    kind: Kind,
    state: &str,
    query: &str,
    counts: ListCounts,
    items: &[Item],
    filters: &ListFilters,
) -> Element {
    let url = repo.family_url(kind);
    let plural = kind.title().to_lowercase();
    repositories::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-list", aria_label: kind.title(),
                div { class: "collaboration-toolbar",
                    div { class: "collaboration-title-block",
                        h2 { "{kind.title()}" }
                        p { if kind == Kind::Issue { "Plan work and discuss ideas." } else { "Review and merge proposed changes." } }
                    }
                    div { class: "collaboration-toolbar-actions",
                        NavLink { class: "btn btn-secondary", href: format!("{}/milestones", repo.url()), "Milestones" }
                        {contribute(repo, kind)}
                    }
                }
                div { class: "collaboration-panel collaboration-list-panel",
                    header { class: "collaboration-panel-header collaboration-list-header",
                        nav { class: "collaboration-state-filters", aria_label: "Entry state",
                            for (value, title, count) in [("open", "Open", counts.open), ("closed", "Closed", counts.closed), ("all", "All", counts.all)] {
                                NavLink { href: list_href(&url, value, query, filters), class: if state == value { "collaboration-filter-link collaboration-filter-active" } else { "collaboration-filter-link" },
                                    if value != "all" { {item_icon(kind, value)} }
                                    span { class: "collaboration-filter-count", "{count}" }
                                    span { aria_current: if state == value { "page" } else { "false" }, "{title}" }
                                }
                            }
                        }
                        if !items.is_empty() { span { class: "collaboration-muted", "{items.len()} shown" } }
                    }
                    {filter_bar(&url, kind, state, query, filters)}
                    {filter_chips(&url, state, query, filters)}
                    if items.is_empty() {
                        div { class: "collaboration-empty",
                            {item_icon(kind, state)}
                            h3 { "{empty_title(kind, state, query)}" }
                            p {
                                if !query.is_empty() { "Try another search, or clear it to see every {plural}." }
                                else if state == "closed" { "Closed {kind.singular()} conversations will appear here." }
                                else if kind == Kind::Issue { "Track bugs, share ideas, and discuss work with your collaborators." }
                                else { "Propose changes and review them with your collaborators." }
                            }
                            if !query.is_empty() { NavLink { href: list_href(&url, state, "", filters), "Clear search" } }
                            else if state != "all" { NavLink { href: format!("{url}?state=all"), "View all {kind.title().to_lowercase()}" } }
                        }
                    }
                    {rows(&url, kind, items)}
                }
                if items.len() >= 100 { p { class: "collaboration-limit collaboration-muted", "Showing the latest 100 matching {kind.title().to_lowercase()}." } }
            }
        },
    )
}

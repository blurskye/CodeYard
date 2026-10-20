use super::{
    detail::{ago, author_link, glyph, item_icon, labels, Glyph},
    filter_menu::{Choice, FilterMenu, Mark},
    label_hex,
    search::search_text,
    search_box::SearchBox,
    Item, Kind, ListCounts, ListFilters, Repository,
};
use crate::webui::{
    components::count_label, components::Avatar, encode_component, navigation::NavLink,
    pages::repos,
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

/// The state a row's icon shows. An open pull request that is still a draft has its own icon.
fn row_state(kind: Kind, item: &Item) -> &str {
    let draft = kind == Kind::Pull
        && item.state == "open"
        && item
            .comparison
            .as_ref()
            .is_some_and(|comparison| comparison.draft);
    if draft {
        "draft"
    } else {
        item.state.as_str()
    }
}

/// The reviewers' state of a pull request row: a check mark when approved, a cross when changes
/// are requested. The tooltip names the state.
fn review_mark(state: &str) -> Element {
    let (label, icon) = match state {
        "approved" => ("Approved", Glyph::Check),
        _ => ("Changes requested", Glyph::Cross),
    };
    rsx! {
        span { class: "collaboration-row-state collaboration-row-state-{state}", title: label.to_owned(), role: "img", aria_label: label.to_owned(), {glyph(icon)} }
    }
}

/// The checks state of a pull request row: a check mark when every run passed, a ring while they
/// run, and a cross when one did not pass. The tooltip names the state.
fn check_mark(state: &str) -> Element {
    let (label, icon) = match state {
        "success" => ("Checks passed", Glyph::Check),
        "pending" => ("Checks running", Glyph::Pending),
        _ => ("Checks failed", Glyph::Cross),
    };
    rsx! {
        span { class: "collaboration-row-state collaboration-row-state-{state}", title: label.to_owned(), role: "img", aria_label: label.to_owned(), {glyph(icon)} }
    }
}

/// The list rows. The first line holds the state icon, the title and the labels. The second line
/// says when it was opened and by whom, then the milestone and the branches. On the right are the
/// comment count and the assignees. Milestone pages show the same rows.
pub(in crate::webui::pages::repos) fn rows(url: &str, kind: Kind, items: &[Item]) -> Element {
    rsx! {
        for item in items {
            article { class: "collaboration-row",
                div { class: "collaboration-row-icon", title: row_state(kind, item).to_owned(), {item_icon(kind, row_state(kind, item))} span { class: "repo-sr-only", "{row_state(kind, item)} {kind.singular()}" } }
                div { class: "collaboration-row-content",
                    div { class: "collaboration-row-title",
                        NavLink { class: "collaboration-title-link", href: format!("{url}/{}", item.number), "{item.title}" }
                        {labels(&item.labels)}
                        if item.tasks.total > 0 { span { class: "collaboration-muted collaboration-task-count", "{item.tasks.done} of {item.tasks.total} tasks" } }
                        if row_state(kind, item) == "draft" { span { class: "collaboration-badge", "Draft" } }
                        if item.stacked { span { class: "collaboration-badge", "Stack" } }
                    }
                    p { class: "collaboration-row-meta", "#{item.number} opened " {ago(&item.created)} " by " {author_link(&item.author)}
                        if let Some(milestone) = &item.milestone { span { class: "collaboration-row-milestone", " · {milestone.title}" } }
                        if let Some(compare) = &item.comparison { span { class: "collaboration-row-branches", " · {compare.head_label} → {compare.base}" } }
                    }
                }
                if item.comment_count > 0 || !item.assignees.is_empty() || item.review_state.is_some() || item.check_state.is_some() {
                    div { class: "collaboration-row-side",
                        if let Some(review) = &item.review_state { {review_mark(review)} }
                        if let Some(check) = &item.check_state { {check_mark(check)} }
                        if item.comment_count > 0 {
                            span { class: "collaboration-row-comments", title: count_label(item.comment_count as usize, "comment", "comments"),
                                {glyph(Glyph::Comment)}
                                "{item.comment_count}"
                            }
                        }
                        for name in &item.assignees {
                            span { class: "collaboration-row-assignee", title: name.clone(), role: "img", aria_label: name.clone(), Avatar { name: name.clone(), size: 20 } }
                        }
                    }
                }
            }
        }
    }
}

/// The search field as a GET form. The box holds the state and every filter as qualifiers, so the
/// text is all the form sends. Enter runs it; there is no separate button. It works without
/// JavaScript, and once hydrated the box suggests qualifiers and values.
fn search_form(url: &str, kind: Kind, state: &str, query: &str, filters: &ListFilters) -> Element {
    let plural = kind.title().to_lowercase();
    let value = search_text(kind, state, filters, query);
    rsx! {
        form { class: "collaboration-search", method: "get", action: url.to_owned(), role: "search", aria_label: "Search {plural}",
            SearchBox { kind, value, filters: filters.clone() }
        }
    }
}

/// A filter menu: a button with the filter's name and a caret, and a panel of choices. It is a
/// native menu, so it opens without scripts. It is marked when a filter in it is set.
pub(in crate::webui::pages::repos) fn filter_menu(
    title: &str,
    set: bool,
    choices: Element,
) -> Element {
    rsx! {
        details { class: if set { "native-menu collaboration-filter-menu collaboration-filter-set" } else { "native-menu collaboration-filter-menu" },
            summary { class: "collaboration-filter-button", span { "{title}" } {glyph(Glyph::Caret)} }
            div { class: "native-menu-panel collaboration-filter-panel", {choices} }
        }
    }
}

/// One choice in a filter menu: a link to the list with that choice made. The choice in use is
/// checked.
pub(in crate::webui::pages::repos) fn choice(href: String, text: &str, chosen: bool) -> Element {
    rsx! {
        NavLink { class: if chosen { "collaboration-filter-option collaboration-filter-chosen" } else { "collaboration-filter-option" }, href: href, aria_current: chosen.then(|| "true".to_owned()),
            span { class: "collaboration-filter-check", aria_hidden: "true", if chosen { {glyph(Glyph::Check)} } }
            span { "{text}" }
        }
    }
}

/// Copies only selected values, never the choice catalogues used to render the menus.
fn selected_filters(filters: &ListFilters) -> ListFilters {
    ListFilters {
        author: filters.author.clone(),
        assignee: filters.assignee.clone(),
        milestone: filters.milestone.clone(),
        sort: filters.sort.clone(),
        selected_labels: filters.selected_labels.clone(),
        ..Default::default()
    }
}

/// The filters with one label added, or removed when it is already chosen.
pub(super) fn toggle_label(filters: &ListFilters, name: &str) -> ListFilters {
    let mut selected = filters.selected_labels.clone();
    match selected.iter().position(|chosen| chosen.as_str() == name) {
        Some(at) => {
            selected.remove(at);
        }
        None => selected.push(name.to_owned()),
    }
    ListFilters {
        selected_labels: selected,
        ..selected_filters(filters)
    }
}

/// The Author, Labels, Milestones, Assignees and Sort menus. Each choice is a link to the list with
/// that filter set, so picking one applies it at once. The labels menu adds or removes one label
/// per link.
fn filter_menus(url: &str, state: &str, query: &str, filters: &ListFilters) -> Element {
    const SORTS: [(&str, &str); 6] = [
        ("newest", "Newest"),
        ("oldest", "Oldest"),
        ("most-commented", "Most commented"),
        ("least-commented", "Least commented"),
        ("recently-updated", "Recently updated"),
        ("least-recently-updated", "Least recently updated"),
    ];
    let href = |change: ListFilters| list_href(url, state, query, &change);
    let sort = if filters.sort.is_empty() {
        "newest"
    } else {
        filters.sort.as_str()
    };
    // The trigger names the order in use, so the choice shows without opening the menu.
    let sort_text = SORTS
        .iter()
        .find(|(value, _)| *value == sort)
        .map_or("Newest", |(_, text)| *text);
    let people = |chosen: &str, set: fn(&ListFilters, String) -> ListFilters, names: &[String]| {
        names
            .iter()
            .map(|name| Choice {
                mark: Mark::Person(name.clone()),
                ..Choice::plain(href(set(filters, name.clone())), name, chosen == name)
            })
            .collect::<Vec<_>>()
    };
    let authors = people(
        &filters.author,
        |f, v| ListFilters {
            author: v,
            ..selected_filters(f)
        },
        &filters.authors,
    );
    let mut assignees = people(
        &filters.assignee,
        |f, v| ListFilters {
            assignee: v,
            ..selected_filters(f)
        },
        &filters.assignees,
    );
    assignees.insert(
        0,
        Choice::plain(
            href(ListFilters {
                assignee: "none".to_owned(),
                ..selected_filters(filters)
            }),
            "Assigned to nobody",
            filters.assignee == "none",
        ),
    );
    let labels: Vec<Choice> = filters
        .labels
        .iter()
        .map(|label| Choice {
            mark: label_hex(&label.color).map_or(Mark::None, Mark::Colour),
            detail: label.description.clone(),
            ..Choice::plain(
                href(toggle_label(filters, &label.name)),
                &label.name,
                filters.selected_labels.contains(&label.name),
            )
        })
        .collect();
    let mut milestones = vec![Choice::plain(
        href(ListFilters {
            milestone: "none".to_owned(),
            ..selected_filters(filters)
        }),
        "No milestone",
        filters.milestone == "none",
    )];
    milestones.extend(filters.milestones.iter().map(|milestone| Choice {
        detail: if milestone.state == "closed" {
            "Closed".to_owned()
        } else {
            String::new()
        },
        ..Choice::plain(
            href(ListFilters {
                milestone: milestone.number.to_string(),
                ..selected_filters(filters)
            }),
            &milestone.title,
            filters.milestone == milestone.number.to_string(),
        )
    }));
    let sorts: Vec<Choice> = SORTS
        .iter()
        .map(|(value, text)| {
            Choice::plain(
                href(ListFilters {
                    sort: (*value).to_owned(),
                    ..selected_filters(filters)
                }),
                text,
                sort == *value,
            )
        })
        .collect();
    let clear = |set: bool, change: ListFilters| set.then(|| href(change));
    rsx! {
        div { class: "collaboration-filter-menus",
            FilterMenu { id: "filter-author", title: "Author", heading: "Filter by author", search_label: "Filter users", searchable: true, set: !filters.author.is_empty(), choices: authors, empty: "No authors yet",
                clear: clear(!filters.author.is_empty(), ListFilters { author: String::new(), ..selected_filters(filters) }) }
            FilterMenu { id: "filter-labels", title: "Labels", heading: "Filter by label", search_label: "Filter labels", searchable: true, set: !filters.selected_labels.is_empty(), choices: labels, empty: "No labels yet",
                clear: clear(!filters.selected_labels.is_empty(), ListFilters { selected_labels: Vec::new(), ..selected_filters(filters) }) }
            FilterMenu { id: "filter-milestones", title: "Milestones", heading: "Filter by milestone", search_label: "Filter milestones", searchable: true, set: !filters.milestone.is_empty(), choices: milestones, empty: "No milestones yet",
                clear: clear(!filters.milestone.is_empty(), ListFilters { milestone: String::new(), ..selected_filters(filters) }) }
            FilterMenu { id: "filter-assignees", title: "Assignees", heading: "Filter by assignee", search_label: "Filter users", searchable: true, set: !filters.assignee.is_empty(), choices: assignees, empty: "No one can be assigned yet",
                clear: clear(!filters.assignee.is_empty(), ListFilters { assignee: String::new(), ..selected_filters(filters) }) }
            FilterMenu { id: "filter-sort", title: format!("Sort: {sort_text}"), heading: "Sort by",search_label: "", searchable: false, set: sort != "newest", choices: sorts, empty: "",
                clear: clear(sort != "newest", ListFilters { sort: String::new(), ..selected_filters(filters) }) }
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
    repos::chrome::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-list", aria_label: kind.title(),
                div { class: "collaboration-search-row",
                    {search_form(&url, kind, state, query, filters)}
                    NavLink { class: "btn btn-secondary", href: format!("{}/labels", repo.url()), "Labels" }
                    NavLink { class: "btn btn-secondary", href: format!("{}/milestones", repo.url()), "Milestones" }
                    {contribute(repo, kind)}
                }
                div { class: "collaboration-panel collaboration-list-panel",
                    header { class: "collaboration-panel-header collaboration-list-header",
                        nav { class: "collaboration-state-filters", aria_label: "Entry state",
                            for (value, title, count) in [("open", "Open", counts.open), ("closed", "Closed", counts.closed)] {
                                NavLink { href: list_href(&url, value, query, filters), class: if state == value { "collaboration-filter-link collaboration-filter-active" } else { "collaboration-filter-link" },
                                    {item_icon(kind, value)}
                                    span { aria_current: if state == value { "page" } else { "false" }, "{count} {title}" }
                                }
                            }
                        }
                        {filter_menus(&url, state, query, filters)}
                    }
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
                            if !query.is_empty() { NavLink { class: "btn btn-secondary", href: list_href(&url, state, "", filters), "Clear search" } }
                            else if state == "open" { {contribute(repo, kind)} }
                        }
                    }
                    {rows(&url, kind, items)}
                }
                if kind == Kind::Issue && (filters.page > 1 || filters.has_more) {
                    nav { class: "collaboration-pagination", aria_label: "Issue pages",
                        if filters.page > 1 { NavLink { class: "btn btn-secondary", href: format!("{}&page={}", list_href(&url, state, query, filters), filters.page - 1), "Previous" } }
                        if filters.has_more { NavLink { class: "btn btn-secondary", href: format!("{}&page={}", list_href(&url, state, query, filters), filters.page + 1), "Next" } }
                    }
                }
                if kind == Kind::Pull && items.len() >= 100 { p { class: "collaboration-limit collaboration-muted", "Showing the latest 100 matching {plural}." } }
            }
        },
    )
}

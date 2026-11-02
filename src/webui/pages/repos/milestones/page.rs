//! Milestone pages: the list, a milestone's page with its issues, and the new-milestone form.
use crate::webui::pages::repos::chrome::csrf;
use crate::webui::pages::repos::issues::filter_menu::{Choice, FilterMenu};
use crate::webui::pages::repos::issues::{
    notice, rows, state_class, Item, Kind, ListCounts, Milestone, MilestoneDraft, Repository,
};
use crate::webui::{
    components::{confirm_dialog, DraftTextarea},
    navigation::NavLink,
    pages::repos,
};
use dioxus::prelude::*;

fn progress(milestone: &Milestone) -> Element {
    let percent = milestone.progress();
    rsx! {
        div { class: "collaboration-progress", role: "progressbar", aria_label: "Progress of {milestone.title}", "aria-valuemin": "0", "aria-valuemax": "100", "aria-valuenow": percent.to_string(),
            span { class: "collaboration-progress-bar", style: format!("width: {percent}%") }
        }
        p { class: "collaboration-muted", "{percent}% complete. {milestone.closed_items} closed, {milestone.open_items} open." }
    }
}

/// The title, description and due date fields of the milestone forms.
fn fields(draft: &MilestoneDraft) -> Element {
    rsx! {
        div { class: "collaboration-form-fields",
            label { class: "collaboration-form-field", "Title"
                input { r#type: "text", name: "title", class: "collaboration-input", required: true, maxlength: "255", placeholder: "Milestone title", value: draft.title.clone() }
            }
            label { class: "collaboration-form-field", "Description (optional)"
                // The shared textarea sets the text as its value. Text written as child nodes would
                // show the rendering markers inside an empty description.
                DraftTextarea { name: "description", initial_value: draft.description.clone(), rows: 5, class: "collaboration-textarea", placeholder: "What the milestone is for" }
            }
            label { class: "collaboration-form-field", "Due date (optional)"
                input { r#type: "date", name: "due_date", class: "collaboration-input", value: draft.due_date.clone() }
            }
        }
    }
}

/// The orders of the milestone list: the value in the address and its title. The first is the default.
pub const MILESTONE_SORTS: [(&str, &str); 8] = [
    ("closest-due", "Closest due date"),
    ("furthest-due", "Furthest due date"),
    ("most-complete", "Most complete"),
    ("least-complete", "Least complete"),
    ("most-issues", "Most issues"),
    ("least-issues", "Least issues"),
    ("alphabetical", "Alphabetically"),
    ("reverse-alphabetical", "Reverse alphabetically"),
];

pub fn list(
    repo: &Repository,
    state: &str,
    counts: ListCounts,
    items: &[Milestone],
    sort: &str,
    page_number: usize,
    has_next: bool,
) -> Element {
    let url = repo.url();
    let sort = if sort.is_empty() {
        MILESTONE_SORTS[0].0
    } else {
        sort
    };
    let sorts: Vec<Choice> = MILESTONE_SORTS
        .iter()
        .map(|(value, title)| {
            let href = if *value == MILESTONE_SORTS[0].0 {
                format!("{url}/milestones?state={state}")
            } else {
                format!("{url}/milestones?state={state}&sort={value}")
            };
            Choice::plain(href, title, sort == *value)
        })
        .collect();
    repos::chrome::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-milestones", aria_label: "Milestones",
                div { class: "collaboration-toolbar",
                    div { class: "collaboration-title-block",
                        h2 { "Milestones" }
                        p { "Group issues into goals and follow how far along they are." }
                    }
                    if repo.signed_in && repo.can_write { NavLink { class: "btn btn-primary", href: format!("{url}/milestones/new"), "New milestone" } }
                }
                div { class: "collaboration-panel collaboration-list-panel",
                    header { class: "collaboration-panel-header collaboration-list-header",
                        nav { class: "collaboration-state-filters", aria_label: "Milestone state",
                            for (value, title, count) in [("open", "Open", counts.open), ("closed", "Closed", counts.closed)] {
                                NavLink { href: format!("{url}/milestones?state={value}"), class: if state == value { "collaboration-filter-link collaboration-filter-active" } else { "collaboration-filter-link" }, aria_current: (state == value).then(|| "page".to_owned()),
                                    span { class: "collaboration-filter-count", "{count}" }
                                    span { "{title}" }
                                }
                            }
                        }
                        div { class: "collaboration-filter-menus",
                            FilterMenu { id: "filter-milestone-sort", title: "Sort", heading: "Sort by", search_label: "", searchable: false, set: sort != MILESTONE_SORTS[0].0, choices: sorts, empty: "",
                                clear: None::<String> }
                        }
                    }
                    if items.is_empty() {
                        div { class: "collaboration-empty",
                            svg { class: "collaboration-empty-icon", view_box: "0 0 24 24", width: "32", height: "32", fill: "none", stroke: "currentColor", stroke_width: "1.5", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true", "focusable": "false",
                                path { d: "M5 21V4M5 4h11l-2 4 2 4H5" }
                            }
                            h3 { if state == "closed" { "No closed milestones" } else { "You have not created any milestones" } }
                            p { "Milestones group the issues and pull requests of a release or a goal." }
                            if repo.signed_in && repo.can_write && state == "open" {
                                NavLink { class: "btn btn-primary", href: format!("{url}/milestones/new"), "New milestone" }
                            }
                        }
                    }
                    for milestone in items {
                        article { class: "collaboration-row collaboration-milestone-row",
                            div { class: "collaboration-row-content",
                                div { class: "collaboration-row-title",
                                    NavLink { class: "collaboration-title-link", href: format!("{url}/milestones/{}", milestone.number), "{milestone.title}" }
                                }
                                if let Some(description) = &milestone.description { p { class: "collaboration-muted", "{description}" } }
                                {progress(milestone)}
                                if let Some(due) = &milestone.due_date { p { class: "collaboration-row-meta", "Due " time { datetime: due.clone(), "{due}" } } }
                            }
                        }
                    }
                }
                nav { class: "repo-pagination", aria_label: "Milestone pages",
                    if page_number > 1 { NavLink { class: "btn btn-secondary", href: format!("{url}/milestones?state={state}&sort={sort}&page={}", page_number - 1), "Previous" } }
                    if has_next { NavLink { class: "btn btn-secondary", href: format!("{url}/milestones?state={state}&sort={sort}&page={}", page_number + 1), "Next" } }
                }
            }
        },
    )
}

pub fn new(repo: &Repository, draft: &MilestoneDraft, error: Option<&str>) -> Element {
    let url = repo.url();
    repos::chrome::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-milestone-new", aria_label: "New milestone",
                {notice(error)}
                header { class: "collaboration-detail-heading", h2 { "New milestone" } }
                form { method: "post", action: format!("{url}/milestones/new"), class: "collaboration-panel collaboration-milestone-form",
                    {csrf(&repo.csrf)}
                    input { r#type: "hidden", name: "repository_id", value: draft.repository_id.clone().unwrap_or_else(|| repo.chrome.id.to_string()) }
                    {fields(draft)}
                    div { class: "collaboration-card-footer",
                        NavLink { class: "btn btn-secondary", href: format!("{url}/milestones"), "Cancel" }
                        button { r#type: "submit", class: "btn btn-primary", "Create milestone" }
                    }
                }
            }
        },
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "The milestone page's rendered fields."
)]
pub fn detail(
    repo: &Repository,
    milestone: &Milestone,
    state: &str,
    items: &[Item],
    pull_requests: &[Item],
    pagination: (usize, bool),
    draft: &MilestoneDraft,
    error: Option<&str>,
) -> Element {
    let url = format!("{}/milestones/{}", repo.url(), milestone.number);
    let issues_url = repo.family_url(Kind::Issue);
    let pulls_url = repo.family_url(Kind::Pull);
    let action = if milestone.state == "open" {
        "close"
    } else {
        "reopen"
    };
    let all = milestone.open_items + milestone.closed_items;
    repos::chrome::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-milestone", aria_label: "Milestone",
                {notice(error)}
                NavLink { class: "collaboration-milestone-back", href: format!("{}/milestones", repo.url()), "← Milestones" }
                header { class: "collaboration-detail-heading",
                    h2 { "{milestone.title} " span { class: "collaboration-number", "#{milestone.number}" } }
                    div { class: "collaboration-detail-meta",
                        span { class: state_class(&milestone.state), "{milestone.state}" }
                        if let Some(due) = &milestone.due_date { span { class: "collaboration-muted", "Due " time { datetime: due.clone(), "{due}" } } }
                    }
                }
                section { class: "collaboration-panel collaboration-milestone-summary",
                    if let Some(description) = &milestone.description { p { class: "collaboration-milestone-description", "{description}" } }
                    {progress(milestone)}
                    if repo.can_write {
                        div { class: "collaboration-card-footer",
                            details { class: "collaboration-edit", open: error.is_some(), title: "Edit milestone",
                                summary { class: "btn btn-secondary btn-sm", "Edit" }
                                form { method: "post", action: format!("{url}/edit"), class: "collaboration-panel collaboration-milestone-form",
                                    {csrf(&repo.csrf)}
                                    input { r#type: "hidden", name: "repository_id", value: repo.chrome.id.to_string() }
                                    {fields(draft)}
                                    div { class: "collaboration-card-footer",
                                        button { r#type: "submit", class: "btn btn-primary", "Save milestone" }
                                    }
                                }
                            }
                            form { method: "post", action: format!("{url}/{action}"), class: "collaboration-state-action",
                                {csrf(&repo.csrf)}
                                input { r#type: "hidden", name: "repository_id", value: repo.chrome.id.to_string() }
                                button { r#type: "submit", class: "btn btn-secondary btn-sm", if action == "close" { "Close milestone" } else { "Reopen milestone" } }
                            }
                            details { class: "collaboration-edit native-menu repo-delete-menu",
                                summary { class: "btn btn-danger btn-sm", "Delete milestone" }
                                {confirm_dialog(None, &format!("Delete milestone {}", milestone.title), &url, &format!("{url}/delete"), rsx! {
                                    {csrf(&repo.csrf)}
                                    {repos::chrome::repository_id(repo.chrome.id)}
                                    input { r#type: "hidden", name: "milestone_id", value: milestone.id.to_string() }
                                    strong { "Delete milestone {milestone.title}?" }
                                    label { class: "repo-check-label", input { r#type: "checkbox", name: "confirm", value: "yes", required: true } " Delete this milestone. Its issues stay open or closed and lose it." }
                                }, "Delete milestone")}
                            }
                        }
                    }
                }
                div { class: "collaboration-panel collaboration-list-panel",
                    header { class: "collaboration-panel-header collaboration-list-header",
                        nav { class: "collaboration-state-filters", aria_label: "Milestone items",
                            for (value, title, count) in [("open", "Open", milestone.open_items), ("closed", "Closed", milestone.closed_items), ("all", "All", all)] {
                                NavLink { href: format!("{url}?state={value}"), class: if state == value { "collaboration-filter-link collaboration-filter-active" } else { "collaboration-filter-link" }, aria_current: (state == value).then(|| "page".to_owned()),
                                    span { class: "collaboration-filter-count", "{count}" }
                                    span { "{title}" }
                                }
                            }
                        }
                        if !items.is_empty() || !pull_requests.is_empty() { span { class: "collaboration-muted", "{items.len() + pull_requests.len()} shown" } }
                    }
                    if items.is_empty() && pull_requests.is_empty() { div { class: "collaboration-empty", h3 { "No issues or pull requests here" } p { "Set a milestone from the sidebar of an issue or pull request to list it here." } } }
                    {rows(&issues_url, Kind::Issue, items)}
                    {rows(&pulls_url, Kind::Pull, pull_requests)}
                    nav { class: "repo-pagination", aria_label: "All milestone items",
                        NavLink { href: format!("{issues_url}?milestone={}&state=all", milestone.number), "View all issues" }
                        NavLink { href: format!("{pulls_url}?milestone={}&state=all", milestone.number), "View all pull requests" }
                    }
                    if pagination.0 > 1 || pagination.1 {
                        nav { class: "collaboration-card-footer", aria_label: "Milestone pagination",
                            if pagination.0 > 1 {
                                NavLink { class: "btn btn-secondary", href: format!("{url}?state={state}&page={}", pagination.0 - 1), "Previous" }
                            }
                            if pagination.1 {
                                NavLink { class: "btn btn-secondary", href: format!("{url}?state={state}&page={}", pagination.0 + 1), "Next" }
                            }
                        }
                    }
                }
            }
        },
    )
}

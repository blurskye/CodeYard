//! Milestone pages: the list, a milestone's page with its issues, and the new-milestone form.
use super::{
    detail::{csrf, notice, state_class},
    list::rows,
    Item, Kind, ListCounts, Milestone, MilestoneDraft, Repository,
};
use crate::webui::{navigation::NavLink, pages::repositories};
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
            label { class: "collaboration-field", "Title"
                input { r#type: "text", name: "title", class: "collaboration-input", required: true, maxlength: "255", value: draft.title.clone() }
            }
            label { class: "collaboration-field", "Description"
                textarea { name: "description", class: "collaboration-textarea", rows: "5", "{draft.description}" }
            }
            label { class: "collaboration-field", "Due date (optional)"
                input { r#type: "date", name: "due_date", class: "collaboration-input", value: draft.due_date.clone() }
            }
        }
    }
}

pub fn list(repo: &Repository, state: &str, counts: ListCounts, items: &[Milestone]) -> Element {
    let url = repo.url();
    repositories::chrome(
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
                                NavLink { href: format!("{url}/milestones?state={value}"), class: if state == value { "collaboration-filter-link collaboration-filter-active" } else { "collaboration-filter-link" },
                                    span { class: "collaboration-filter-count", "{count}" }
                                    span { aria_current: if state == value { "page" } else { "false" }, "{title}" }
                                }
                            }
                        }
                    }
                    if items.is_empty() {
                        div { class: "collaboration-empty",
                            h3 { if state == "closed" { "No closed milestones" } else { "No open milestones" } }
                            p { "Milestones group the issues of a release or a goal." }
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
            }
        },
    )
}

pub fn new(repo: &Repository, draft: &MilestoneDraft, error: Option<&str>) -> Element {
    let url = repo.url();
    repositories::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-milestone-new", aria_label: "New milestone",
                {notice(error)}
                header { class: "collaboration-detail-heading", h2 { "New milestone" } }
                form { method: "post", action: format!("{url}/milestones/new"), class: "collaboration-panel collaboration-milestone-form",
                    {csrf(&repo.csrf)}
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

pub fn detail(
    repo: &Repository,
    milestone: &Milestone,
    state: &str,
    items: &[Item],
    pull_requests: &[Item],
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
    repositories::chrome(
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
                                    {fields(draft)}
                                    div { class: "collaboration-card-footer",
                                        button { r#type: "submit", class: "btn btn-primary", "Save milestone" }
                                    }
                                }
                            }
                            form { method: "post", action: format!("{url}/{action}"), class: "collaboration-state-action",
                                {csrf(&repo.csrf)}
                                button { r#type: "submit", class: "btn btn-secondary btn-sm", if action == "close" { "Close milestone" } else { "Reopen milestone" } }
                            }
                            form { method: "post", action: format!("{url}/delete"), class: "collaboration-state-action",
                                {csrf(&repo.csrf)}
                                button { r#type: "submit", class: "btn btn-danger btn-sm", "Delete milestone" }
                                span { class: "collaboration-muted", " Its issues stay open or closed and lose this milestone." }
                            }
                        }
                    }
                }
                div { class: "collaboration-panel collaboration-list-panel",
                    header { class: "collaboration-panel-header collaboration-list-header",
                        nav { class: "collaboration-state-filters", aria_label: "Milestone items",
                            for (value, title, count) in [("open", "Open", milestone.open_items), ("closed", "Closed", milestone.closed_items), ("all", "All", all)] {
                                NavLink { href: format!("{url}?state={value}"), class: if state == value { "collaboration-filter-link collaboration-filter-active" } else { "collaboration-filter-link" },
                                    span { class: "collaboration-filter-count", "{count}" }
                                    span { aria_current: if state == value { "page" } else { "false" }, "{title}" }
                                }
                            }
                        }
                        if !items.is_empty() || !pull_requests.is_empty() { span { class: "collaboration-muted", "{items.len() + pull_requests.len()} shown" } }
                    }
                    if items.is_empty() && pull_requests.is_empty() { div { class: "collaboration-empty", h3 { "No issues or pull requests here" } p { "Set a milestone from the sidebar of an issue or pull request to list it here." } } }
                    {rows(&issues_url, Kind::Issue, items)}
                    {rows(&pulls_url, Kind::Pull, pull_requests)}
                }
            }
        },
    )
}

use crate::webui::{
    components::{confirm_dialog, count_label, DraftInput},
    encode_component,
    pages::repos::{
        self,
        issues::{
            csrf,
            detail::label_chip,
            label_hex,
            list::{choice, filter_menu},
            Label, Repository,
        },
    },
};
use dioxus::prelude::*;
use std::collections::BTreeMap;

/// The background of a color swatch. Only a validated `#rrggbb` value reaches the style.
fn swatch_style(color: &str) -> String {
    label_hex(color)
        .map(|hex| format!("background-color: {hex}"))
        .unwrap_or_else(|| "background-color: var(--muted)".to_owned())
}

/// The color field of the label forms. The hex text is what gets posted; the swatch beside it
/// shows the color as it is typed.
#[component]
fn LabelColorInput(initial_value: String) -> Element {
    let mut color = use_signal(|| initial_value.clone());
    let swatch = swatch_style(&color());
    rsx! {
        span { class: "repo-color-field",
            span { class: "repo-color-swatch", style: "{swatch}", aria_hidden: "true" }
            DraftInput {
                name: "color",
                required: true,
                initial_value,
                pattern: Some("#?[0-9a-fA-F]{6}".to_owned()),
                on_value: move |value: String| color.set(value),
            }
        }
    }
}

/// The name, color and description fields, shared by the new-label and edit forms.
fn label_fields(name: String, color: String, description: String) -> Element {
    rsx! {
        div { class: "collaboration-label-fields",
            label { span { "Name" } DraftInput { name: "name", required: true, maxlength: Some(50), initial_value: name, placeholder: "Label name" } }
            label { span { "Color" } LabelColorInput { initial_value: color } }
            label { span { "Description" } DraftInput { name: "description", maxlength: Some(1024), initial_value: description, placeholder: "What is this label for?" } }
        }
    }
}

/// The edit form of one label, under its Edit button.
fn edit_label(repo: &Repository, label: &Label) -> Element {
    rsx! {
        details { class: "collaboration-edit",
            summary { class: "btn btn-secondary btn-sm", "Edit" span { class: "repo-sr-only", " {label.name}" } }
            form { method: "post", action: repo.chrome.action("edit-label"), class: "collaboration-form-fields",
                {csrf(&repo.csrf)}
                input { r#type: "hidden", name: "label_id", value: label.id.to_string() }
                {label_fields(label.name.clone(), label.color.clone(), label.description.clone())}
                div { class: "collaboration-actions", button { r#type: "submit", class: "btn btn-primary", "Save label" } }
            }
        }
    }
}

/// The delete button of one label. It opens a confirmation that needs its box ticked.
fn delete_label(repo: &Repository, label: &Label) -> Element {
    let list_url = format!("{}/labels", repo.url());
    rsx! {
        details { class: "collaboration-edit native-menu repo-delete-menu",
            summary { class: "btn btn-danger btn-sm", "Delete" span { class: "repo-sr-only", " {label.name}" } }
            {confirm_dialog(None, &format!("Delete label {}", label.name), &list_url, &repo.chrome.action("delete-label"), rsx! {
                {csrf(&repo.csrf)}
                input { r#type: "hidden", name: "label_id", value: label.id.to_string() }
                strong { "Delete label {label.name}?" }
                label { class: "repo-check-label", input { r#type: "checkbox", name: "confirm", value: "yes", required: true } " Remove this label from every issue and pull request" }
            }, "Delete label")}
        }
    }
}

/// The usage of a label as its row shows it: how many issues and pull requests carry it.
fn usage_text(count: i64) -> String {
    count_label(
        count as usize,
        "issue or pull request",
        "issues and pull requests",
    )
}

/// The labels page. The search matches a label's name and description. The sort is by name unless
/// the newest, the oldest or the most used label is asked for. `usage` counts, by label id, the
/// issues and pull requests each label is on.
pub fn list(
    repo: &Repository,
    items: &[Label],
    query: &str,
    sort: &str,
    usage: &BTreeMap<i64, i64>,
    page_number: usize,
    has_next: bool,
) -> Element {
    let can_admin = repo.can_admin;
    let url = format!("{}/labels", repo.url());
    let sort = if sort.is_empty() { "name" } else { sort };
    let needle = query.trim().to_lowercase();
    // The server filters the complete label set before paging it.
    let mut shown: Vec<&Label> = items.iter().collect();
    match sort {
        "newest" => shown.sort_by_key(|label| std::cmp::Reverse(label.id)),
        "oldest" => shown.sort_by_key(|label| label.id),
        "most-used" => shown.sort_by_key(|label| {
            (
                std::cmp::Reverse(usage.get(&label.id).copied().unwrap_or(0)),
                label.name.to_lowercase(),
            )
        }),
        _ => shown.sort_by_key(|label| label.name.to_lowercase()),
    }
    let none_match = !needle.is_empty() && shown.is_empty();
    let count = count_label(shown.len(), "label", "labels");
    let sorts = [
        ("name", "Name"),
        ("newest", "Newest"),
        ("oldest", "Oldest"),
        ("most-used", "Most used"),
    ];
    let sort_text = sorts
        .iter()
        .find(|(value, _)| *value == sort)
        .map_or("Name", |(_, text)| *text);
    let sort_href =
        |value: &str| format!("{url}?q={}&sort={value}", encode_component(query.trim()));
    repos::chrome::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-labels-page", aria_label: "Labels",
                if page_number > 1 || has_next {
                    nav { class: "repo-pagination", aria_label: "Label pages",
                        if page_number > 1 {
                            crate::webui::navigation::NavLink { href: format!("{}&page={}", sort_href(sort), page_number - 1), "Previous" }
                        }
                        if has_next {
                            crate::webui::navigation::NavLink { href: format!("{}&page={}", sort_href(sort), page_number + 1), "Next" }
                        }
                    }
                }
                div { class: "collaboration-toolbar",
                    div { class: "collaboration-title-block",
                        h2 { "Labels" }
                        p { "Group issues and pull requests by topic." }
                    }
                    if can_admin {
                        // A native menu: the trigger stays where it is and the form opens beneath it.
                        details { class: "native-menu collaboration-label-new",
                            summary { class: "btn btn-primary", "New label" }
                            div { class: "native-menu-panel collaboration-label-new-panel",
                                form { method: "post", action: repo.chrome.action("label"), class: "collaboration-label-new-form",
                                    {csrf(&repo.csrf)}
                                    div { class: "collaboration-form-fields",
                                        {label_fields(String::new(), "#ededed".to_owned(), String::new())}
                                        div { class: "collaboration-actions",
                                            a { class: "btn btn-secondary", href: sort_href(sort), "Cancel" }
                                            button { r#type: "submit", class: "btn btn-primary", "Create label" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "collaboration-search-row",
                    form { class: "collaboration-search", method: "get", action: url.clone(), role: "search", aria_label: "Search labels",
                        input { r#type: "hidden", name: "sort", value: sort.to_owned() }
                        input { r#type: "search", name: "q", class: "collaboration-input", value: query.to_owned(), maxlength: "200", placeholder: "Search all labels", aria_label: "Search all labels" }
                    }
                    div { class: "collaboration-filter-menus",
                        {filter_menu(&format!("Sort: {sort_text}"), sort != "name", rsx! {
                            for (value, text) in sorts {
                                {choice(sort_href(value), text, sort == value)}
                            }
                        })}
                    }
                }
                div { class: "collaboration-panel collaboration-list-panel",
                    header { class: "collaboration-panel-header collaboration-list-header",
                        h3 { class: "collaboration-count", "{count}" }
                    }
                    if items.is_empty() {
                        div { class: "collaboration-empty",
                            h3 { "No labels yet" }
                            p { if can_admin { "Create a label to sort issues and pull requests by topic." } else { "This repository has no labels yet." } }
                        }
                    }
                    if none_match {
                        div { class: "collaboration-empty", h3 { "No labels match your search" } }
                    }
                    for label in shown {
                        article { key: "{label.id}", class: "collaboration-row collaboration-label-row",
                            div { class: "collaboration-row-content",
                                div { class: "collaboration-row-title", {label_chip(label)} }
                                if !label.description.is_empty() { p { class: "collaboration-muted", "{label.description}" } }
                            }
                            div { class: "collaboration-row-side",
                                span { {usage_text(usage.get(&label.id).copied().unwrap_or(0))} }
                            }
                            if can_admin {
                                div { class: "collaboration-row-side collaboration-label-actions",
                                    {edit_label(repo, label)}
                                    {delete_label(repo, label)}
                                }
                            }
                        }
                    }
                }
            }
        },
    )
}

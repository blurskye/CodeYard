use super::Page;
use crate::webui::components::{DraftInput, DraftSelect, DraftTextarea, SelectOption};
use dioxus::prelude::*;

pub(super) const FIELD: &str = "org-field";
/// What a field starts with: the rejected submission's value when that form was posted,
/// otherwise the stored value.
pub(super) fn field_value(page: &Page, action: &str, name: &str) -> String {
    let stored = || {
        if action == "profile" {
            page.selected
                .as_ref()
                .map(|org| match name {
                    "display_name" => org.display_name.clone().unwrap_or_default(),
                    "description" => org.description.clone(),
                    _ => String::new(),
                })
                .unwrap_or_default()
        } else if let Some(detail) = &page.selected_team {
            match (action, name) {
                (_, "team_id") => detail.team.id.to_string(),
                ("team-edit", "name") => detail.team.name.clone(),
                ("team-edit", "description") => detail.description.clone(),
                ("team-edit", "privacy") => detail.team.privacy.clone(),
                _ => String::new(),
            }
        } else {
            String::new()
        }
    };
    if page.draft.get("action").is_some_and(|v| v == action) {
        page.draft.get(name).cloned().unwrap_or_else(stored)
    } else {
        stored()
    }
}
pub(super) fn input(page: &Page, action: &str, name: &str, label: &str) -> Element {
    let value = field_value(page, action, name);
    let limit = match name {
        "description" => 4096,
        "user_id" | "team_id" | "repo_id" | "org_id" => 32,
        _ => 1024,
    };
    rsx! { label { span { "{label}" }
        if name == "description" {
            DraftTextarea { class: FIELD, name: name.to_owned(), initial_value: value, maxlength: Some(limit), rows: 4 }
        } else {
            DraftInput { class: FIELD, name: name.to_owned(), initial_value: value, maxlength: Some(limit) }
        }
    } }
}
/// One option of a fixed-choice select: the value posted and the label people read.
pub(super) fn choice(value: &str, label: &str) -> SelectOption {
    SelectOption {
        value: value.to_owned(),
        label: label.to_owned(),
    }
}
/// A fixed-choice field. `initial_value` comes from `field_value`, or from the row itself
/// when one form repeats for each row.
pub(super) fn select(
    label: &str,
    name: &str,
    initial_value: String,
    options: Vec<SelectOption>,
) -> Element {
    rsx! { label { span { "{label}" }
        DraftSelect { class: FIELD, name: name.to_owned(), initial_value, options, required: true }
    } }
}
/// The field a removal row asks for: the removed account's username, typed to confirm.
pub(super) fn removal_confirm(username: &str) -> Element {
    rsx! { label { span { "Type {username} to confirm" }
        DraftInput { class: FIELD, name: "confirm".to_owned(), initial_value: String::new(), maxlength: Some(1024) }
    } }
}
/// The hidden CSRF and action fields, and the form's primary button.
pub(super) fn controls(page: &Page, action: &str, label: &str) -> Element {
    controls_as(page, action, label, "btn btn-primary")
}
/// As `controls`, with the button in another button class (for example a row action).
pub(super) fn controls_as(page: &Page, action: &str, label: &str, class: &str) -> Element {
    rsx! {
        input { r#type: "hidden", name: "csrf_token", value: page.csrf_token.clone() }
        input { r#type: "hidden", name: "action", value: action.to_owned() }
        button { r#type: "submit", class: class.to_owned(), "{label}" }
    }
}

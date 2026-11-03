use super::{
    page::{IssueAttribute, IssueAttributes},
    Repository,
};
use crate::{
    components::{DraftCheckbox, DraftInput, DraftSelect, DraftTextarea, SelectOption},
    pages::repos::chrome::csrf,
};
use dioxus::prelude::*;
const COLORS: [&str; 8] = [
    "gray", "blue", "green", "yellow", "orange", "red", "pink", "purple",
];
fn color(name: &str, value: Option<&str>, nullable: bool) -> Element {
    let mut options = Vec::new();
    if nullable {
        options.push(SelectOption::new("", "None"));
    }
    options.extend(COLORS.map(|choice| SelectOption::new(choice, choice)));
    rsx! {
        DraftSelect {
            name: name.to_owned(),
            class: "collaboration-input",
            options,
            initial_value: value.unwrap_or(if nullable { "" } else { "gray" }).to_owned(),
        }
    }
}
fn identity_fields(repo: &Repository, item_id: i64, owner_key: &str) -> Element {
    rsx! {
        {csrf(&repo.csrf)}
        input { r#type: "hidden", name: "repository_id", value: "{repo.chrome.id}" }
        input { r#type: "hidden", name: "item_id", value: "{item_id}" }
        input { r#type: "hidden", name: "owner_key", value: owner_key.to_owned() }
    }
}

fn definition(
    repo: &Repository,
    url: &str,
    a: &IssueAttribute,
    (item_id, owner_key): (i64, &str),
) -> Element {
    let csrf = |_: &str| identity_fields(repo, item_id, owner_key);
    let field = a.kind == "field";
    let kind = if field { "field" } else { "type" };
    rsx! {
        div {
            key: "{url}:definition:{kind}:{a.id}",
            class: "collaboration-form-fields",
            form {
                method: "post",
                action: format!("{url}/attributes"),
                class: "collaboration-form-fields",
                {csrf(&repo.csrf)}
                input { r#type: "hidden", name: "action", value: "configure" }
                input { r#type: "hidden", name: "expected_revision", value: a.revision.clone() }
                input { r#type: "hidden", name: "kind", value: kind }
                if a.id > 0 {
                    input {
                        r#type: "hidden",
                        name: "attribute_id",
                        value: a.id.to_string(),
                    }
                    input { r#type: "hidden", name: "expected_updated", value: a.updated.clone() }
                }
                label {
                    "Name"
                    DraftInput {
                        name: "name",
                        initial_value: a.name.clone(),
                        required: true,
                        maxlength: Some(128),
                        class: "collaboration-input",
                    }
                }
                label {
                    "Description"
                    DraftTextarea {
                        name: "description",
                        initial_value: a.description.clone().unwrap_or_default(),
                        maxlength: Some(1024),
                        rows: 2,
                        class: "collaboration-input",
                    }
                }
                if field {
                    if a.id == 0 {
                        label {
                            "Data type"
                            DraftSelect {
                                name: "data_type",
                                class: "collaboration-input",
                                initial_value: a.data_type.clone(),
                                options: ["text", "number", "date", "single_select", "multi_select"]
                                    .map(|v| SelectOption::new(v, v))
                                    .to_vec(),
                            }
                        }
                    } else {
                        input {
                            r#type: "hidden",
                            name: "data_type",
                            value: a.data_type.clone(),
                        }
                        p { class: "collaboration-muted", "Data type: {a.data_type}" }
                    }
                    label {
                        "Visibility"
                        DraftSelect {
                            name: "visibility",
                            class: "collaboration-input",
                            initial_value: a.visibility.clone(),
                            options: vec![
                                SelectOption::new("organization_members_only", "Organization members"),
                                SelectOption::new("all", "Everyone who can read issues"),
                            ],
                        }
                    }
                    for option in &a.options {
                        fieldset { key: "{url}:option:{option.id}",
                            legend { "Select option" }
                            label {
                                "Name"
                                DraftInput {
                                    name: format!("option_{}_name", option.id),
                                    initial_value: option.name.clone(),
                                    required: true,
                                    class: "collaboration-input",
                                }
                            }
                            label {
                                "Color"
                                {color(&format!("option_{}_color", option.id), Some(&option.color), false)}
                            }
                            label {
                                "Description"
                                DraftTextarea {
                                    name: format!("option_{}_description", option.id),
                                    initial_value: option.description.clone().unwrap_or_default(),
                                    maxlength: Some(1024),
                                    rows: 2,
                                    class: "collaboration-input",
                                }
                            }
                            label {
                                "Priority"
                                input {
                                    r#type: "number",
                                    name: format!("option_{}_priority", option.id),
                                    value: option.priority.to_string(),
                                    step: "1",
                                    class: "collaboration-input",
                                }
                            }
                            label {
                                DraftCheckbox {
                                    name: format!("option_{}_remove", option.id),
                                    submission_value: "true",
                                    initial_checked: false,
                                }
                                " Remove option"
                            }
                        }
                    }
                    if a.id == 0 || matches!(a.data_type.as_str(), "single_select" | "multi_select") {
                        label {
                            "New select option (select fields only)"
                            DraftInput {
                                name: "new_option_name",
                                initial_value: "",
                                class: "collaboration-input",
                            }
                        }
                        label {
                            "New option color"
                            {color("new_option_color", Some("gray"), false)}
                        }
                        label {
                            "New option description"
                            DraftTextarea {
                                name: "new_option_description",
                                initial_value: "",
                                maxlength: Some(1024),
                                rows: 2,
                                class: "collaboration-input",
                            }
                        }
                        label {
                            "New option priority"
                            input {
                                r#type: "number",
                                name: "new_option_priority",
                                value: a.options.iter().map(|option| option.priority).max().unwrap_or(-1).saturating_add(1).to_string(),
                                step: "1",
                                class: "collaboration-input",
                            }
                        }
                    }
                } else {
                    label {
                        "Color"
                        {color("color", a.color.as_deref(), true)}
                    }
                    label {
                        DraftCheckbox {
                            name: "is_enabled",
                            submission_value: "true",
                            initial_checked: a.is_enabled,
                        }
                        " Enabled"
                    }
                }
                button { r#type: "submit", class: "btn btn-secondary btn-sm",
                    if a.id == 0 {
                        "Create {kind}"
                    } else {
                        "Save {kind}"
                    }
                }
            }
            if a.id > 0 {
                form { method: "post", action: format!("{url}/attributes"),
                    {csrf(&repo.csrf)}
                    input {
                        r#type: "hidden",
                        name: "action",
                        value: "delete_definition",
                    }
                    input {
                        r#type: "hidden",
                        name: "attribute_id",
                        value: a.id.to_string(),
                    }
                    label {
                        DraftCheckbox {
                            name: "confirm",
                            submission_value: "true",
                            initial_checked: false,
                            required: true,
                        }
                        " Remove this {kind} and all its assignments"
                    }
                    button { r#type: "submit", class: "btn btn-danger btn-sm", "Delete {kind}" }
                }
            }
        }
    }
}
fn field_value(a: &IssueAttribute) -> String {
    let Some(value) = &a.value else {
        return "No value".into();
    };
    match a.data_type.as_str() {
        "single_select" => a
            .options
            .iter()
            .find(|o| Some(o.id) == value.as_i64())
            .map(|o| o.name.clone())
            .unwrap_or_else(|| "No value".into()),
        "multi_select" => a
            .options
            .iter()
            .filter(|o| {
                value
                    .as_array()
                    .is_some_and(|v| v.contains(&serde_json::Value::from(o.id)))
            })
            .map(|o| o.name.clone())
            .collect::<Vec<_>>()
            .join(", "),
        _ => value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string()),
    }
}
fn blank(kind: &str) -> IssueAttribute {
    IssueAttribute {
        revision: String::new(),
        id: 0,
        updated: String::new(),
        kind: kind.into(),
        name: String::new(),
        description: None,
        data_type: if kind == "field" {
            "text".into()
        } else {
            String::new()
        },
        visibility: "organization_members_only".into(),
        is_enabled: true,
        color: None,
        options: Vec::new(),
        value: None,
    }
}
pub(super) fn panel(repo: &Repository, url: &str, data: &IssueAttributes, item_id: i64) -> Element {
    let csrf = |_: &str| identity_fields(repo, item_id, &data.owner_key);
    let writer = repo.can_write && !repo.chrome.is_archived;
    let selected = data
        .types
        .iter()
        .find(|a| Some(a.id) == data.selected_type)
        .map(|a| a.name.as_str())
        .unwrap_or("None");
    let type_options = std::iter::once(SelectOption::new("", "None"))
        .chain(
            data.types
                .iter()
                .filter(|a| a.is_enabled || Some(a.id) == data.selected_type)
                .map(|a| SelectOption::new(a.id.to_string(), &a.name)),
        )
        .collect::<Vec<_>>();
    rsx! {
        section {
            key: "{url}:attributes",
            class: "collaboration-sidebar-section",
            aria_label: "Issue attributes",
            h3 { "Issue type" }
            p { class: "collaboration-muted", "{selected}" }
            if writer && !data.types.is_empty() {
                form { method: "post", action: format!("{url}/attributes"),
                    {csrf(&repo.csrf)}
                    input { r#type: "hidden", name: "action", value: "set_type" }
                    label {
                        "Type"
                        DraftSelect {
                            name: "type_id",
                            class: "collaboration-input",
                            initial_value: data.selected_type.map(|id| id.to_string()).unwrap_or_default(),
                            options: type_options,
                        }
                    }
                    button { r#type: "submit", class: "btn btn-secondary btn-sm", "Set type" }
                }
            }
            if !data.fields.is_empty() {
                h3 { "Fields" }
            }
            for a in &data.fields {
                details {
                    key: "{url}:field:{a.id}",
                    class: "collaboration-sidebar-section",
                    summary { "{a.name}: {field_value(a)}" }
                    if writer {
                        form {
                            method: "post",
                            action: format!("{url}/attributes"),
                            class: "collaboration-form-fields",
                            {csrf(&repo.csrf)}
                            input {
                                r#type: "hidden",
                                name: "action",
                                value: "set_value",
                            }
                            input {
                                r#type: "hidden",
                                name: "attribute_id",
                                value: a.id.to_string(),
                            }
                            if a.data_type == "single_select" {
                                label {
                                    "Value"
                                    DraftSelect {
                                        name: "value",
                                        class: "collaboration-input",
                                        initial_value: a.options
                                            .iter()
                                            .find(|o| a.value.as_ref().and_then(|v| v.as_i64()) == Some(o.id))
                                            .or(a.options.first())
                                            .map(|o| o.id.to_string())
                                            .unwrap_or_default(),
                                        // Options are sent by id, so a renamed option on a stale
                                        // page is not mistaken for another.
                                        options: a.options.iter().map(|o| SelectOption::new(o.id.to_string(), &o.name)).collect::<Vec<_>>(),
                                    }
                                }
                            } else if a.data_type == "multi_select" {
                                for option in &a.options {
                                    label { key: "{url}:selection:{option.id}",
                                        DraftCheckbox {
                                            name: format!("option_{}", option.id),
                                            submission_value: "true",
                                            initial_checked: a.value
                                                .as_ref()
                                                .and_then(|v| v.as_array())
                                                .is_some_and(|v| v.contains(&serde_json::Value::from(option.id))),
                                        }
                                        " {option.name}"
                                    }
                                }
                            } else if a.data_type == "text" {
                                label {
                                    "Value"
                                    DraftTextarea {
                                        name: "value",
                                        class: "collaboration-input",
                                        rows: 2,
                                        initial_value: a.value.as_ref().and_then(|v| v.as_str()).unwrap_or("").to_owned(),
                                    }
                                }
                            } else {
                                label {
                                    "Value"
                                    DraftInput {
                                        name: "value",
                                        class: "collaboration-input",
                                        input_type: if a.data_type == "date" { "date" } else { "number" },
                                        step: Some("any".to_owned()),
                                        initial_value: a.value
                                            .as_ref()
                                            .map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()))
                                            .unwrap_or_default(),
                                        required: true,
                                    }
                                }
                            }
                            button {
                                r#type: "submit",
                                class: "btn btn-secondary btn-sm",
                                "Save value"
                            }
                        }
                        if a.value.is_some() {
                            form {
                                method: "post",
                                action: format!("{url}/attributes"),
                                {csrf(&repo.csrf)}
                                input {
                                    r#type: "hidden",
                                    name: "action",
                                    value: "delete_value",
                                }
                                input {
                                    r#type: "hidden",
                                    name: "attribute_id",
                                    value: a.id.to_string(),
                                }
                                button {
                                    r#type: "submit",
                                    class: "btn btn-secondary btn-sm",
                                    "Clear value"
                                }
                            }
                        }
                    }
                }
            }
            if data.can_configure && writer {
                details {
                    summary { "Manage owner attributes" }
                    p { class: "collaboration-muted",
                        "Changes here apply to every repository owned by this account."
                    }
                    for a in data.types.iter().chain(data.fields.iter()) {
                        details { key: "{url}:manage:{a.id}",
                            summary { "Edit {a.kind}: {a.name}" }
                            {definition(repo, url, a, (item_id, &data.owner_key))}
                        }
                    }
                    details {
                        summary { "New issue type" }
                        {definition(repo, url, &blank("type"), (item_id, &data.owner_key))}
                    }
                    if data.organization {
                        details {
                            summary { "New issue field" }
                            {definition(repo, url, &blank("field"), (item_id, &data.owner_key))}
                        }
                    }
                }
            }
        }
    }
}

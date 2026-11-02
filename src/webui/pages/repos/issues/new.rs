use super::{
    detail::{label_chip, label_option_text, notice, picker_heading, PickerPopover, PickerRow},
    editor::MarkdownEditor,
    Kind, Label, Milestone, Repository,
};
use crate::webui::{
    components::{
        references::References, DraftCheckbox, DraftInput, DraftSelect, DraftTextarea, Markdown,
        SelectOption,
    },
    encode_component,
    navigation::NavLink,
    pages::{
        repos,
        repos::chrome::csrf,
        repos::pulls::diff::{diff_summary, diff_view, Diff},
    },
};
use dioxus::prelude::*;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NewDraft {
    #[serde(default)]
    pub repository_id: String,
    #[serde(default)]
    pub destination_changed: bool,
    #[serde(default)]
    pub template_revision: String,
    pub head_repo: String,
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    pub draft: bool,
    /// The assignees, labels and milestone a writer picked for a new issue.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub milestone: String,
    /// The path of the issue form this issue is written with, and the form's fields. A form
    /// replaces the description: its answers become the body when the issue is created.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub template: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub form: Vec<FormField>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FieldKind {
    Markdown,
    #[default]
    Input,
    Textarea,
    Dropdown,
    Checkboxes,
}

/// One field of an issue form with the answer given so far.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FormField {
    pub kind: FieldKind,
    pub label: String,
    pub description: String,
    pub placeholder: String,
    /// The text of a Markdown element.
    pub text: String,
    /// The labels of a choice's options.
    pub options: Vec<String>,
    pub multiple: bool,
    pub required: bool,
    /// The answer: the typed text, or the labels of the chosen options.
    pub values: Vec<String>,
}
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BranchChoices {
    pub repositories: Vec<(i64, String)>,
    pub base: Vec<String>,
    pub head: Vec<String>,
}
/// What a writer can pick on a new issue: the users who may be assigned, the labels and the
/// milestones. Readers and pull requests get none.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct IssueChoices {
    pub assignable: Vec<String>,
    pub labels: Vec<Label>,
    pub milestones: Vec<Milestone>,
}
/// The form that holds the new entry's fields. The issue sidebar sits beside it, so its inputs
/// name the form instead of sitting inside it.
const NEW_FORM: &str = "new-entry-form";

/// One issue template in the chooser. `path` names it in the link that opens the form.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TemplateCard {
    pub path: String,
    pub name: String,
    pub about: String,
}

/// The template chooser a new issue opens on when the repository has issue templates.
pub fn chooser(repo: &Repository, templates: &[TemplateCard], blank_allowed: bool) -> Element {
    let url = format!("{}/new", repo.family_url(Kind::Issue));
    repos::chrome::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-new collaboration-template-chooser",
                header { class: "collaboration-page-heading", h2 { "New issue" } }
                p { class: "collaboration-muted", "Choose a template to start from." }
                ul { class: "collaboration-template-list",
                    for template in templates {
                        li {
                            NavLink { class: "collaboration-template-card collaboration-panel", href: format!("{url}?template={}", encode_component(&template.path)),
                                strong { "{template.name}" }
                                if !template.about.is_empty() { span { class: "collaboration-muted", "{template.about}" } }
                            }
                        }
                    }
                }
                if blank_allowed {
                    NavLink { class: "btn btn-secondary", href: format!("{url}?template="), "Open a blank issue" }
                }
            }
        },
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "Each field of the new-entry page model is a parameter; a struct would only wrap them."
)]
pub fn new(
    repo: &Repository,
    kind: Kind,
    draft: &NewDraft,
    branches: &BranchChoices,
    choices: &IssueChoices,
    diff: Option<&Diff>,
    error: Option<&str>,
    preview: bool,
    references: &References,
) -> Element {
    let url = format!("{}/new", repo.family_url(kind));
    // Writers pick assignees, labels and a milestone on an issue; the sidebar is not shown to others.
    let picks = kind == Kind::Issue && repo.can_write;
    let (title_hint, body_hint) = if kind == Kind::Issue {
        (
            "A short summary of the issue",
            "Describe the problem, the steps to reproduce it, or the idea. Markdown is supported.",
        )
    } else {
        (
            "A short summary of the change",
            "Explain what changes and why. Markdown is supported.",
        )
    };
    repos::chrome::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-new",
                header { class: "collaboration-page-heading", h2 { "New {kind.singular()}" } }
                {notice(error)}
                div { class: if picks { "collaboration-new-layout" } else { "collaboration-new-layout collaboration-new-plain" },
                    div { class: "collaboration-new-main",
                        form { id: NEW_FORM, method: "post", action: url, class: "collaboration-panel collaboration-editor",
                            {csrf(&repo.csrf)}
                            // Enter in the title creates; without this, the Compare or Preview button
                            // that comes first would take it.
                            button { r#type: "submit", name: "intent", value: "create", class: "sr-only", tabindex: "-1", aria_hidden: "true" }
                            if kind == Kind::Issue {
                                input { r#type: "hidden", name: "repository_id", value: draft.repository_id.clone() }
                                input { r#type: "hidden", name: "template", value: draft.template.clone() }
                                if !draft.template_revision.is_empty() {
                                    input { r#type: "hidden", name: "template_revision", value: draft.template_revision.clone() }
                                }
                            }
                            if kind == Kind::Pull {
                                div { class: "collaboration-comparison-controls",
                                    div { class: "collaboration-compare-row",
                                        {branch_select("base", "Base branch", &draft.base, &branches.base)}
                                        svg { class: "collaboration-compare-arrow", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round", width: "18", height: "18", "aria-hidden": "true", "focusable": "false",
                                            path { d: "M19 12H5M12 19l-7-7 7-7" }
                                        }
                                        label { class: "collaboration-branch-select", "Head repository"
                                            DraftSelect { name: "head_repo", initial_value: draft.head_repo.clone(), options: branches.repositories.iter().map(|(id, label)| SelectOption { value: id.to_string(), label: label.clone() }).collect::<Vec<_>>() }
                                        }
                                        {branch_select("head", "Compare branch", &draft.head, &branches.head)}
                                        button { r#type: "submit", name: "intent", value: "compare", formnovalidate: true, class: "btn btn-primary", "Compare" }
                                    }
                                    if diff.is_none() {
                                        p { class: "collaboration-panel collaboration-muted collaboration-compare-hint", "Choose the branch with your changes and the branch to merge into. Compare shows the changes before you open the pull request." }
                                    }
                                }
                            }
                            div { class: "collaboration-form-fields",
                                label { r#for: "new-title", "Title " span { class: "collaboration-required", "Required" } }
                                DraftInput { id: "new-title", name: "title", required: true, maxlength: Some(255), placeholder: title_hint, initial_value: draft.title.clone(), class: "collaboration-input" }
                                if draft.form.is_empty() {
                                    div { key: "{draft.repository_id}:{draft.template}:{draft.template_revision}:new:{draft.body}:{preview}", class: "collaboration-form-field",
                                        label { r#for: "new-body", "Description (Markdown)" }
                                        MarkdownEditor { id: "new-body", initial: draft.body.clone(), rows: 12, required: false, initial_preview: preview, placeholder: body_hint, references: references.clone() }
                                    }
                                } else {
                                    for (index, field) in draft.form.iter().enumerate() {
                                        {form_field(index, field)}
                                    }
                                }
                                if kind == Kind::Pull {
                                    label { class: "collaboration-checkbox", DraftCheckbox { name: "draft", initial_checked: draft.draft } " Create as draft" }
                                }
                                if let Some(diff) = diff { {diff_summary(diff)} }
                                div { class: "collaboration-actions",
                                    button { r#type: "submit", name: "intent", value: "create", class: "btn btn-primary", if draft.destination_changed { "Confirm destination and create issue" } else { "Create {kind.singular()}" } }
                                    NavLink { class: "btn btn-secondary", href: repo.family_url(kind), "Cancel" }
                                }
                            }
                        }
                    }
                    if picks {
                        aside { class: "collaboration-new-help collaboration-sidebar", aria_label: "Issue details",
                            IssuePickers { key: "{draft.repository_id}:{draft.template}:{draft.template_revision}", draft: draft.clone(), choices: choices.clone(), labels_url: format!("{}/labels", repo.url()), can_label: repo.can_admin }
                        }
                    }
                }
                if let Some(diff) = diff { {diff_view(diff, false)} }
            }
        },
    )
}
/// One field of an issue form. Its answers are posted as `form_<index>`, in the order of the
/// file, so the server can write them into the issue body.
fn form_field(index: usize, field: &FormField) -> Element {
    let name = format!("form_{index}");
    let id = format!("form-field-{index}");
    let first = field.values.first().cloned().unwrap_or_default();
    if field.kind == FieldKind::Markdown {
        return rsx! { div { class: "collaboration-muted collaboration-form-text collaboration-markdown", Markdown { content: field.text.clone() } } };
    }
    // A group of checkboxes has no single control for a label to name, so a legend names it.
    if field.kind == FieldKind::Checkboxes || (field.kind == FieldKind::Dropdown && field.multiple)
    {
        return rsx! {
            fieldset { class: "collaboration-form-field collaboration-option-group",
                legend { "{field.label}"
                    if field.required { " " span { class: "collaboration-required", "Required" } }
                }
                if !field.description.is_empty() { p { class: "collaboration-muted collaboration-field-hint", "{field.description}" } }
                div { class: "collaboration-form-checks",
                    for option in field.options.iter() {
                        label { key: "{option}", class: "collaboration-checkbox",
                            DraftCheckbox { name: name.clone(), initial_checked: field.values.contains(option), submission_value: option.clone() }
                            " {option}"
                        }
                    }
                }
            }
        };
    }
    rsx! {
        div { class: "collaboration-form-field",
            label { r#for: id.clone(), "{field.label}"
                if field.required { " " span { class: "collaboration-required", "Required" } }
            }
            if !field.description.is_empty() { p { class: "collaboration-muted collaboration-field-hint", "{field.description}" } }
            match field.kind {
                FieldKind::Input => rsx! {
                    DraftInput { id: id.clone(), name, required: field.required, maxlength: Some(255), placeholder: field.placeholder.clone(), initial_value: first, class: "collaboration-input" }
                },
                FieldKind::Textarea => rsx! {
                    DraftTextarea { id: id.clone(), name, required: field.required, rows: 6, placeholder: field.placeholder.clone(), initial_value: first, class: "collaboration-input" }
                },
                _ => rsx! {
                    DraftSelect { id: id.clone(), name, required: field.required, initial_value: first, options: field.options.iter().map(|label| SelectOption::new(label.clone(), label.clone())).collect::<Vec<_>>() }
                },
            }
        }
    }
}

/// The assignee, label and milestone pickers of a new issue. Each menu names the form beside this
/// sidebar, so the picks are sent with the title and body when the issue is created. The picks live
/// in signals, so the summaries under the headings follow the menus. Writers only.
#[component]
fn IssuePickers(
    draft: NewDraft,
    choices: IssueChoices,
    labels_url: String,
    can_label: bool,
) -> Element {
    let assignees = use_signal(|| draft.assignees.clone());
    let labels = use_signal(|| draft.labels.clone());
    let milestone = use_signal(|| draft.milestone.clone());
    let assignee_text = if assignees().is_empty() {
        "No one".to_owned()
    } else {
        assignees().join(", ")
    };
    let picked = labels();
    let chosen: Vec<&Label> = choices
        .labels
        .iter()
        .filter(|entry| picked.contains(&entry.id.to_string()))
        .collect();
    let none_chosen = chosen.is_empty();
    let milestone_text = choices
        .milestones
        .iter()
        .find(|entry| entry.number.to_string() == milestone())
        .map_or("No milestone".to_owned(), |entry| entry.title.clone());
    rsx! {
        section { class: "collaboration-sidebar-section",
            {picker_heading("Assignees", Some(assignee_checks(&choices, assignees)))}
            p { class: "collaboration-muted", "{assignee_text}" }
        }
        if can_label {
            section { class: "collaboration-sidebar-section",
                {picker_heading("Labels", Some(label_checks(&labels_url, &choices, labels)))}
                div { class: "collaboration-labels",
                    for entry in chosen { {label_chip(entry)} }
                }
                if none_chosen { p { class: "collaboration-muted", "None yet" } }
            }
        }
        section { class: "collaboration-sidebar-section",
            {picker_heading("Milestone", Some(milestone_choices(&choices, milestone)))}
            p { class: "collaboration-muted", "{milestone_text}" }
        }
    }
}

/// Adds `value` to the picks or removes it, as its checkbox says.
fn set_picked(mut picks: Signal<Vec<String>>, value: &str, checked: bool) {
    let mut list = picks.write();
    list.retain(|entry| entry != value);
    if checked {
        list.push(value.to_owned());
    }
}

/// The assignee checkboxes of the new issue's picker.
fn assignee_checks(choices: &IssueChoices, assignees: Signal<Vec<String>>) -> Element {
    rsx! {
        PickerPopover { title: "Assign people to this issue",
            if choices.assignable.is_empty() { p { class: "collaboration-muted", "No one to assign." } }
            ul { class: "collaboration-picker-list",
                for name in &choices.assignable {
                    PickerRow { key: "{name}", text: name.clone(),
                        label { class: "collaboration-picker-option",
                            input { r#type: "checkbox", name: "assignees", value: name.clone(), form: NEW_FORM, checked: assignees().contains(name),
                                oninput: {
                                    let name = name.clone();
                                    move |event: FormEvent| set_picked(assignees, &name, event.checked())
                                }
                            }
                            span { "{name}" }
                        }
                    }
                }
            }
        }
    }
}

/// The label checkboxes of the new issue's picker, with a link to the labels page.
fn label_checks(labels_url: &str, choices: &IssueChoices, labels: Signal<Vec<String>>) -> Element {
    rsx! {
        PickerPopover { title: "Apply labels to this issue",
            p { class: "collaboration-muted", "Choose at most 100 labels." }
            if choices.labels.is_empty() { p { class: "collaboration-muted", "No labels yet." } }
            ul { class: "collaboration-picker-list",
                for entry in &choices.labels {
                    PickerRow { key: "{entry.id}", text: entry.name.clone(),
                        label { class: "collaboration-picker-option",
                            input { r#type: "checkbox", name: "labels", value: entry.id.to_string(), form: NEW_FORM, checked: labels().contains(&entry.id.to_string()),
                                oninput: {
                                    let id = entry.id.to_string();
                                    move |event: FormEvent| set_picked(labels, &id, event.checked())
                                }
                            }
                            {label_option_text(entry)}
                        }
                    }
                }
            }
            NavLink { class: "collaboration-manage-labels", href: labels_url.to_owned(), "Edit labels" }
        }
    }
}

/// The milestone radio buttons of the new issue's picker. "No milestone" is the first choice.
fn milestone_choices(choices: &IssueChoices, mut milestone: Signal<String>) -> Element {
    rsx! {
        PickerPopover { title: "Set milestone",
            ul { class: "collaboration-picker-list",
                PickerRow { text: "No milestone".to_owned(),
                    label { class: "collaboration-picker-option",
                        input { r#type: "radio", name: "milestone", value: "", form: NEW_FORM, checked: milestone().is_empty(),
                            oninput: move |_| milestone.set(String::new())
                        }
                        span { "No milestone" }
                    }
                }
                for entry in &choices.milestones {
                    PickerRow { key: "{entry.number}", text: entry.title.clone(),
                        label { class: "collaboration-picker-option",
                            input { r#type: "radio", name: "milestone", value: entry.number.to_string(), form: NEW_FORM, checked: milestone() == entry.number.to_string(),
                                oninput: {
                                    let number = entry.number.to_string();
                                    move |_| milestone.set(number.clone())
                                }
                            }
                            span { "{entry.title}" }
                        }
                    }
                }
            }
        }
    }
}

pub(in crate::webui::pages::repos) fn branch_select(
    name: &str,
    title: &str,
    chosen: &str,
    branches: &[String],
) -> Element {
    let mut options = Vec::new();
    if !chosen.is_empty() && !branches.iter().any(|branch| branch == chosen) {
        options.push(SelectOption {
            value: chosen.to_owned(),
            label: format!("{chosen} (unavailable)"),
        });
    }
    options.extend(branches.iter().map(|branch| SelectOption {
        value: branch.clone(),
        label: branch.clone(),
    }));
    rsx! {
        label { class: "collaboration-branch-select", "{title}"
            DraftSelect { name: name.to_owned(), initial_value: chosen.to_owned(), options }
        }
    }
}

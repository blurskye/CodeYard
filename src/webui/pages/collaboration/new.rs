use super::{
    detail::{csrf, notice},
    diff::diff_view,
    editor::MarkdownEditor,
    label_style, Diff, Kind, Label, Milestone, Repository,
};
use crate::webui::{
    components::{references::References, DraftCheckbox, DraftInput, DraftSelect, SelectOption},
    navigation::NavLink,
    pages::repositories,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NewDraft {
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
    repositories::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-new",
                header { class: "collaboration-page-heading", h2 { "New {kind.singular()}" } }
                {notice(error)}
                div { class: "collaboration-new-layout",
                    div { class: "collaboration-new-main",
                form { id: NEW_FORM, method: "post", action: url, class: "collaboration-panel collaboration-editor",
                    {csrf(&repo.csrf)}
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
                        label { r#for: "new-title", "Title" }
                        DraftInput { id: "new-title", name: "title", required: true, maxlength: Some(255), initial_value: draft.title.clone(), class: "collaboration-input" }
                        label { r#for: "new-body", "Description (Markdown)" }
                        MarkdownEditor { key: "{repo.url()}:new:{draft.body}:{preview}", id: "new-body", initial: draft.body.clone(), rows: 12, required: false, initial_preview: preview, references: references.clone() }
                        if kind == Kind::Pull {
                            label { class: "collaboration-checkbox", DraftCheckbox { name: "draft", initial_checked: draft.draft } " Create as draft" }
                        }
                        div { class: "collaboration-actions",
                            button { r#type: "submit", name: "intent", value: "create", class: "btn btn-primary", "Create {kind.singular()}" }
                            NavLink { class: "btn btn-secondary", href: repo.family_url(kind), "Cancel" }
                        }
                    }
                }
                    }
                    aside { class: "collaboration-new-help collaboration-sidebar", aria_label: if picks { "Issue details" } else { "Writing guidance" },
                        if picks {
                            {issue_pickers(draft, choices, repo.can_admin)}
                        }
                        section { class: "collaboration-sidebar-section",
                            h3 { if kind == Kind::Issue { "Start a useful discussion" } else { "Make review easier" } }
                            ul {
                                li { if kind == Kind::Issue { "Use a clear title that describes the work." } else { "Summarize the change and why it is needed." } }
                                li { "Add context, steps, or decisions in the description." }
                                li { if kind == Kind::Issue { "Avoid including passwords or other secrets." } else { "Create a draft if the change is not ready for review." } }
                            }
                        }
                    }
                }
                if let Some(diff) = diff { {diff_view(diff)} }
            }
        },
    )
}
/// The assignee, label and milestone pickers of a new issue. Each names the form beside this
/// sidebar, so the picks are sent with the title and body. Writers only.
fn issue_pickers(draft: &NewDraft, choices: &IssueChoices, can_label: bool) -> Element {
    rsx! {
        section { class: "collaboration-sidebar-section",
            h3 { "Assignees" }
            if choices.assignable.is_empty() { p { class: "collaboration-muted", "No one to assign." } }
            div { class: "collaboration-choice-list",
                for name in &choices.assignable {
                    label { class: "collaboration-choice",
                        input { r#type: "checkbox", name: "assignees", value: name.clone(), form: NEW_FORM, checked: draft.assignees.contains(name) }
                        span { "{name}" }
                    }
                }
            }
        }
        if can_label {
        section { class: "collaboration-sidebar-section",
            h3 { "Labels" }
            if choices.labels.is_empty() { p { class: "collaboration-muted", "No labels yet." } }
            div { class: "collaboration-choice-list",
                for entry in &choices.labels {
                    label { class: "collaboration-choice",
                        input { r#type: "checkbox", name: "labels", value: entry.id.to_string(), form: NEW_FORM, checked: draft.labels.contains(&entry.id.to_string()) }
                        span { class: "collaboration-label", style: label_style(&entry.color), "{entry.name}" }
                    }
                }
            }
        }
        }
        section { class: "collaboration-sidebar-section",
            h3 { "Milestone" }
            label { class: "collaboration-choice-select", span { class: "repo-sr-only", "Milestone" }
                DraftSelect { name: "milestone", form: Some(NEW_FORM.to_owned()), class: "collaboration-input", initial_value: draft.milestone.clone(),
                    options: std::iter::once(SelectOption::new("", "No milestone"))
                        .chain(choices.milestones.iter().map(|milestone| SelectOption::new(milestone.number.to_string(), milestone.title.clone())))
                        .collect::<Vec<_>>() }
            }
        }
    }
}

pub(super) fn branch_select(name: &str, title: &str, chosen: &str, branches: &[String]) -> Element {
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

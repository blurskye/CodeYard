use super::{
    runs::{icon, repository_frame, Workflow},
    ActionsPage,
};
use crate::webui::{
    components::{DraftCheckbox, DraftSelect, DraftTextarea, SelectOption},
    navigation::NavLink,
    pages::repos::chrome::RepoChrome,
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DispatchDraft {
    pub owner: String,
    pub repository: String,
    pub branch: String,
    pub event: String,
    pub pipeline: String,
    pub workflow: String,
    pub head_sha: String,
    pub environment: String,
    /// Repository form: each typed `input:<path>:<name>` value, kept when a run is refused.
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    /// Global form: the `name=value` lines as typed, kept when a run is refused.
    #[serde(default)]
    pub inputs_text: String,
}
impl Default for DispatchDraft {
    fn default() -> Self {
        Self {
            owner: String::new(),
            repository: String::new(),
            branch: "main".into(),
            event: "workflow_dispatch".into(),
            pipeline: "ci".into(),
            workflow: String::new(),
            head_sha: String::new(),
            environment: String::new(),
            inputs: BTreeMap::new(),
            inputs_text: String::new(),
        }
    }
}

/// The events the server accepts for a manual trigger (`ci::is_supported_event`).
pub const TRIGGER_EVENTS: [&str; 3] = ["workflow_dispatch", "push", "pull_request"];

/// One input a workflow declares for its manual run, as the form shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowInput {
    pub name: String,
    pub description: String,
    pub kind: InputKind,
    pub required: bool,
    pub default: Option<String>,
    pub options: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Text,
    Boolean,
    Number,
    Choice,
    Environment,
}

/// The form field for one input. Its name carries the workflow path, so the inputs of every
/// dispatchable workflow can share one form and the server reads only the chosen workflow's.
/// A checkbox sends nothing when unchecked, so a hidden `false` comes first; the checked value
/// later in the form overrides it. `values` holds what was typed on a refused run; without it the
/// defaults show. `html_required` is set only when the form lists a single workflow: the browser
/// then blocks an empty required field. Otherwise the field says so, and the server enforces it.
pub(super) fn input_field(
    path: &str,
    input: &WorkflowInput,
    values: &BTreeMap<String, String>,
    html_required: bool,
) -> Element {
    let name = format!("input:{path}:{}", input.name);
    let default = input.default.clone().unwrap_or_default();
    let typed = values.get(&name).cloned();
    let label_text = input.name.clone();
    let description = input.description.clone();
    let required = html_required && input.required && input.kind != InputKind::Boolean;
    let show_required_label = input.required && !required;
    match input.kind {
        InputKind::Boolean => {
            let checked = typed.map_or(default == "true", |value| value == "true");
            rsx! {
                label { class: "actions-input-check",
                    input { r#type: "hidden", name: name.clone(), value: "false" }
                    DraftCheckbox { name: name.clone(), submission_value: "true", initial_checked: checked }
                    span { "{label_text}" }
                    if show_required_label { small { " (required)" } }
                    if !description.is_empty() { small { " {description}" } }
                }
            }
        }
        InputKind::Choice | InputKind::Environment if !input.options.is_empty() => {
            let initial = typed
                .or(input.default.clone())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| input.options[0].clone());
            let options = input
                .options
                .iter()
                .map(|option| SelectOption::new(option.clone(), option.clone()))
                .collect::<Vec<_>>();
            rsx! {
                label { "{label_text}"
                    if show_required_label { small { " (required)" } }
                    if !description.is_empty() { small { " {description}" } }
                    DraftSelect { name: name.clone(), initial_value: initial, options: options, required: required }
                }
            }
        }
        InputKind::Number => {
            let current = typed.unwrap_or(default);
            rsx! {
                label { "{label_text}"
                    if show_required_label { small { " (required)" } }
                    if !description.is_empty() { small { " {description}" } }
                    input { r#type: "number", step: "any", name: name.clone(), "value": current, maxlength: "2048", required: required }
                }
            }
        }
        InputKind::Text | InputKind::Choice | InputKind::Environment => {
            let current = typed.unwrap_or(default);
            rsx! {
                label { "{label_text}"
                    if show_required_label { small { " (required)" } }
                    if !description.is_empty() { small { " {description}" } }
                    input { name: name.clone(), "value": current, maxlength: "2048", required: required }
                }
            }
        }
    }
}

/// What the repository's Run workflow menu shows. The toolbar menu and the refusal page both use it.
pub(super) struct DispatchMenu<'a> {
    pub base_path: &'a str,
    pub csrf_token: &'a str,
    /// Sent back so a successful run returns to this list view; absent on the refusal page.
    pub return_query: Option<String>,
    pub branch: &'a str,
    pub workflow: &'a str,
    pub environment: &'a str,
    pub workflows: &'a [Workflow],
    pub inputs: &'a BTreeMap<String, String>,
    pub open: bool,
}

pub(super) fn dispatch_menu(menu: DispatchMenu<'_>) -> Element {
    let dispatchable: Vec<&Workflow> = menu
        .workflows
        .iter()
        .filter(|workflow| workflow.dispatchable)
        .collect();
    // A form that lists one workflow marks its required inputs in HTML.
    let html_required = dispatchable.len() == 1;
    rsx! {
        details { class: "native-menu actions-dispatch", open: menu.open,
            summary { class: "actions-button", "Run workflow" span { " ▾" } }
            if dispatchable.is_empty() {
                div { class: "actions-dispatch-menu", p { class: "actions-dispatch-note", "No workflows can be run manually from this branch or tag." } }
            } else {
                form { method: "post", action: format!("{}/dispatch", menu.base_path), class: "actions-dispatch-menu",
                    input { r#type: "hidden", name: "csrf_token", value: menu.csrf_token.to_owned() }
                    if let Some(return_query) = menu.return_query.clone() {
                        input { r#type: "hidden", name: "return_query", value: return_query }
                    }
                    p { class: "actions-dispatch-note", "Choose a workflow and the branch or tag to run it from." }
                    label { "Branch or tag" input { class: "actions-branch-input", name: "branch", "value": menu.branch.to_owned(), required: true, maxlength: "255" } }
                    label { "Workflow" DraftSelect { name: "workflow_path", required: true, initial_value: menu.workflow.to_owned(),
                        options: dispatchable.iter().map(|workflow| SelectOption::new(workflow.path.clone(), workflow.name.to_string())).collect::<Vec<_>>() } }
                    label { "Environment (deployment workflows)" input { name: "environment", "value": menu.environment.to_owned(), maxlength: "100" } }
                    for workflow in dispatchable.iter().filter(|workflow| !workflow.inputs.is_empty()) {
                        fieldset { class: "actions-dispatch-inputs",
                            legend { "{workflow.name} inputs" }
                            for input in &workflow.inputs {
                                {input_field(&workflow.path, input, menu.inputs, html_required)}
                            }
                        }
                    }
                    button { class: "actions-button actions-dispatch-submit", r#type: "submit", {icon("play")} "Run workflow" }
                }
            }
        }
    }
}

pub(in crate::webui::pages) fn recovery(
    repo: Option<&RepoChrome>,
    base_path: &str,
    csrf_token: &str,
    message: &str,
    draft: &DispatchDraft,
    workflows: &[Workflow],
) -> Element {
    let body = rsx! {
        section { class: "actions-page actions-dispatch-recovery",
            div { class: "actions-recovery-content",
                NavLink { class: "actions-back", href: base_path.to_string(), "← Back to workflow runs" }
                h1 { "Run was not accepted" }
                p { role: "alert", class: "actions-error", "{message}" }
                if repo.is_some() {
                    {dispatch_menu(DispatchMenu {
                        base_path,
                        csrf_token,
                        return_query: None,
                        branch: &draft.branch,
                        workflow: &draft.workflow,
                        environment: &draft.environment,
                        workflows,
                        inputs: &draft.inputs,
                        open: true,
                    })}
                } else {
                    {manual_dispatch_form(base_path, csrf_token, draft)}
                }
            }
        }
    };
    repository_frame(repo, body)
}

pub(super) fn manual_dispatch_form(
    base_path: &str,
    csrf_token: &str,
    draft: &DispatchDraft,
) -> Element {
    rsx! {
        details { class: "native-menu actions-dispatch actions-manual-dispatch", open: !draft.owner.is_empty(),
            summary { class: "actions-button", "New run" span { " ▾" } }
            form { method: "post", action: format!("{base_path}/dispatch"), class: "actions-dispatch-menu",
                p { class: "actions-dispatch-note", "Choose a repository and workflow to run." }
                input { r#type: "hidden", name: "csrf_token", value: csrf_token }
                label { "Repository owner" input { name: "repo_owner", "value": draft.owner.clone(), required: true } }
                label { "Repository name" input { name: "repo_name", "value": draft.repository.clone(), required: true, maxlength: "100" } }
                label { "Branch" input { name: "branch", "value": draft.branch.clone(), required: true, maxlength: "255" } }
                label { "Event" DraftSelect { name: "event", initial_value: draft.event.clone(),
                    options: TRIGGER_EVENTS.iter().map(|event| SelectOption::new(*event, *event)).collect::<Vec<_>>() } }
                label { "Pipeline" DraftSelect { name: "pipeline", initial_value: draft.pipeline.clone(),
                    options: ["ci", "cd", "full"].iter().map(|pipeline| SelectOption::new(*pipeline, *pipeline)).collect::<Vec<_>>() } }
                label { "Workflow path (optional)" input { name: "workflow_path", "value": draft.workflow.clone(), maxlength: "500" } }
                label { "Commit SHA (optional)" input { name: "head_sha", "value": draft.head_sha.clone(), maxlength: "40" } }
                label { "Environment (optional)" input { name: "environment", "value": draft.environment.clone(), maxlength: "100" } }
                label { "Inputs (optional)"
                    DraftTextarea { name: "inputs", initial_value: draft.inputs_text.clone(), rows: 4 }
                }
                p { class: "actions-dispatch-note", "For a workflow that declares inputs: one name=value per line. The server checks names, choices and types, and reports any missing required input." }
                button { r#type: "submit", class: "actions-button actions-dispatch-submit", {icon("play")} "Run workflow" }
            }
        }
    }
}

pub(super) fn dispatch_form(model: &ActionsPage) -> Element {
    dispatch_menu(DispatchMenu {
        base_path: &model.base_path,
        csrf_token: &model.csrf_token,
        return_query: Some(model.url(&[])),
        branch: &model.dispatch_branch,
        workflow: model.value("workflow"),
        environment: "",
        workflows: &model.workflows,
        inputs: &BTreeMap::new(),
        open: false,
    })
}

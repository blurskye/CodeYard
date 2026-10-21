use super::{
    dispatch::{dispatch_form, manual_dispatch_form, WorkflowInput},
    logs,
    run::{display_workflow, run_duration, selected_run, short_id},
    DispatchDraft, Run, SelectedRun,
};
use crate::webui::{
    components::{short_sha, DraftSelect, LiveTime, SelectOption},
    encode_component,
    navigation::{NavLink, RefreshLink},
    pages::repos::{self, chrome::RepoChrome},
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workflow {
    pub path: String,
    pub name: String,
    pub dispatchable: bool,
    /// The inputs a manual run of this workflow takes; empty unless it is dispatchable.
    #[serde(default)]
    pub inputs: Vec<WorkflowInput>,
}

/// Safe request-specific data only; usable by the coordinated hydration owner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionsPage {
    /// Authorized repository identity for the shared header; global CI has none.
    #[serde(default)]
    pub repository: Option<(String, String)>,
    /// Shared repository chrome loaded after server-side scope authorization.
    #[serde(default)]
    pub repo_chrome: Option<RepoChrome>,
    pub title: String,
    pub base_path: String,
    pub csrf_token: String,
    pub query: BTreeMap<String, String>,
    pub runs: Vec<Run>,
    pub workflows: Vec<Workflow>,
    pub selected: Option<SelectedRun>,
    pub dispatch_branch: String,
    pub can_dispatch: bool,
    #[serde(default)]
    pub can_manage_runs: bool,
    #[serde(default)]
    pub can_cancel: bool,
    #[serde(default)]
    pub workflow_warning: Option<String>,
    pub run_limit: usize,
    #[serde(default)]
    pub next_before: Option<i64>,
    pub log_limit: usize,
}

impl ActionsPage {
    pub fn value(&self, key: &str) -> &str {
        self.query.get(key).map(String::as_str).unwrap_or("")
    }

    pub fn url(&self, changes: &[(&str, &str)]) -> String {
        query_url(&self.base_path, &self.query, changes)
    }
}

pub fn query_url(base: &str, query: &BTreeMap<String, String>, changes: &[(&str, &str)]) -> String {
    let mut query = query.clone();
    if changes.iter().any(|(key, _)| {
        matches!(
            *key,
            "workflow" | "event" | "status" | "branch" | "actor" | "q"
        )
    }) {
        query.remove("before");
    }
    for (key, value) in changes {
        if value.is_empty() {
            query.remove(*key);
        } else {
            query.insert((*key).into(), (*value).into());
        }
    }
    let encoded = query
        .iter()
        .map(|(key, value)| format!("{}={}", encode_component(key), encode_component(value)))
        .collect::<Vec<_>>()
        .join("&");
    if encoded.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{encoded}")
    }
}

pub fn workflow_name(path: &str) -> String {
    path.split(',')
        .next()
        .unwrap_or(path)
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".yaml")
        .trim_end_matches(".yml")
        .to_string()
}

pub fn matches_filters(run: &Run, query: &BTreeMap<String, String>) -> bool {
    let value = |key: &str| query.get(key).map(String::as_str).unwrap_or("");
    (value("workflow").is_empty()
        || run
            .workflow_path
            .split(',')
            .any(|path| path.trim() == value("workflow")))
        && (value("event").is_empty() || run.event == value("event"))
        && (value("branch").is_empty() || run.branch == value("branch"))
        && (value("status").is_empty()
            || logs::normalize_status(&run.status) == logs::normalize_status(value("status")))
        && (value("q").is_empty()
            || format!(
                "{} {} {} {} {}",
                run.id,
                run.repository,
                run.workflow_path,
                run.branch,
                run.head_sha.as_deref().unwrap_or("")
            )
            .to_lowercase()
            .contains(&value("q").to_lowercase()))
}

pub(super) fn status_label(status: &str) -> &'static str {
    match logs::normalize_status(status) {
        "success" => "Success",
        "failed" => "Failed",
        "running" => "In progress",
        "queued" => "Queued",
        "cancelled" => "Cancelled",
        "skipped" => "Skipped",
        _ => "Unknown",
    }
}

pub(super) fn status_icon(status: &str) -> Element {
    let normalized = logs::normalize_status(status);
    let class = if normalized == "running" {
        "ci-status-running actions-status-spinner".to_string()
    } else {
        format!("ci-status-{normalized}")
    };
    rsx! {
        svg { class, width: "16", height: "16", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true",
            if normalized == "success" {
                circle { cx: "12", cy: "12", r: "10", fill: "currentColor", stroke: "none" }
                path { d: "m9 12 2 2 4-4", stroke: "var(--actions-status-check, var(--background))" }
            } else if normalized == "failed" {
                circle { cx: "12", cy: "12", r: "10" }
                path { d: "m15 9-6 6M9 9l6 6" }
            } else if normalized == "running" {
                path { d: "M21 12a9 9 0 1 1-6.219-8.56" }
            } else if normalized == "skipped" {
                circle { cx: "12", cy: "12", r: "10" }
                path { d: "m5 19 14-14" }
            } else {
                circle { cx: "12", cy: "12", r: "10" }
                path { d: "M12 6v6h4" }
            }
        }
    }
}

/// A status as its icon and label in one chip. The run list, the run header and the summary all
/// show a status this way.
pub(super) fn status_chip(status: &str) -> Element {
    let class = format!(
        "actions-status-chip ci-status-{}",
        logs::normalize_status(status)
    );
    rsx! {
        span { class, {status_icon(status)} span { "{status_label(status)}" } }
    }
}

/// The small icons of the Actions pages. A name that has no icon draws a play triangle.
pub(super) fn icon(kind: &str) -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "1.5", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true",
            match kind {
                "actions" => rsx! { circle { cx: "12", cy: "12", r: "9" } path { d: "m10 8 6 4-6 4z" } },
                "branches" => rsx! { path { d: "M6 3v12M18 9a9 9 0 0 1-9 9" } circle { cx: "18", cy: "6", r: "3" } circle { cx: "6", cy: "18", r: "3" } },
                "summary" => rsx! { path { d: "m3 10 9-7 9 7v10H3zM9 20v-7h6v7" } },
                "usage" => rsx! { path { d: "m12 14 4-4M3.34 19a10 10 0 1 1 17.32 0" } },
                "file" => rsx! { path { d: "M10 12.5 8 15l2 2.5m4-5 2 2.5-2 2.5M14 2v4a2 2 0 0 0 2 2h4M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7z" } },
                "search" => rsx! { circle { cx: "11", cy: "11", r: "8" } path { d: "m21 21-4.3-4.3" } },
                "calendar" => rsx! { rect { x: "3", y: "5", width: "18", height: "16", rx: "1" } path { d: "M3 9h18M7 2v6M17 2v6" } },
                "duration" => rsx! { circle { cx: "12", cy: "14", r: "8" } path { d: "M9 2h6M12 2v4M12 10v4l-3 2" } },
                "wrap" => rsx! { path { d: "M20 7h-9M14 17H5" } circle { cx: "17", cy: "17", r: "3" } circle { cx: "7", cy: "7", r: "3" } },
                _ => rsx! { path { d: "M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z" } },
            }
        }
    }
}

pub(super) fn workflow_icon() -> Element {
    rsx! {
        svg { width: "16", height: "16", view_box: "0 0 16 16", fill: "none", stroke: "currentColor", stroke_width: "1.3", "aria-hidden": "true",
            rect { x: "1", y: "1", width: "5", height: "5", rx: "1" }
            rect { x: "10", y: "5", width: "5", height: "5", rx: "1" }
            rect { x: "1", y: "10", width: "5", height: "5", rx: "1" }
            path { d: "M6 3.5h2v4h2M3.5 6v4" }
        }
    }
}

pub(super) fn repository_base(base_path: &str) -> Option<&str> {
    let base = base_path.strip_suffix("/actions")?;
    let mut segments = base.strip_prefix('/')?.split('/');
    let owner = segments.next()?;
    let repository = segments.next()?;
    (!owner.is_empty() && !repository.is_empty() && segments.next().is_none()).then_some(base)
}

pub(super) fn repository_frame(repo: Option<&RepoChrome>, body: Element) -> Element {
    match repo {
        Some(repo) => rsx! {
            div { class: "actions-repository", {repos::chrome::chrome(repo, body)} }
        },
        None => body,
    }
}

pub(super) fn preserved_inputs(model: &ActionsPage, exclude: &[&str]) -> Element {
    rsx! {
        for (key, value) in model.query.iter().filter(|(key, _)| !exclude.contains(&key.as_str())) {
            input { r#type: "hidden", name: key.clone(), value: value.clone() }
        }
    }
}

/// The directory the CI reads workflow files from (`ci::list_workflows_for_ref`).
const WORKFLOW_DIRECTORY: &str = ".github/workflows";
/// The smallest workflow the runner accepts: `ubuntu-latest` is in the default runner map.
const WORKFLOW_EXAMPLE: &str = "name: CI
on: [push]
jobs:
  hello:
    runs-on: ubuntu-latest
    steps:
      - run: echo \"Hello from Codeyard\"
";

/// The Workflow filter's choices: every workflow on the page, plus the workflow in the URL when the
/// page does not list it.
fn workflow_filter_options(model: &ActionsPage) -> Vec<SelectOption> {
    let chosen = model.value("workflow");
    let mut options = vec![SelectOption::new("", "Workflow")];
    options.extend(
        model
            .workflows
            .iter()
            .map(|workflow| SelectOption::new(workflow.path.clone(), workflow.name.to_string())),
    );
    if !chosen.is_empty()
        && !model
            .workflows
            .iter()
            .any(|workflow| workflow.path == chosen)
    {
        options.push(SelectOption::new(chosen, chosen));
    }
    options
}

/// The Status filter's choices, with the status in the URL when it is not one of them.
fn status_filter_options(model: &ActionsPage) -> Vec<SelectOption> {
    const STATUSES: [&str; 6] = [
        "queued",
        "running",
        "success",
        "failed",
        "cancelled",
        "unknown",
    ];
    let chosen = model.value("status");
    let mut options = vec![SelectOption::new("", "Status")];
    options.extend(
        STATUSES
            .iter()
            .map(|status| SelectOption::new(*status, status_label(status))),
    );
    if !chosen.is_empty() && !STATUSES.contains(&chosen) {
        options.push(SelectOption::new(chosen, chosen));
    }
    options
}

/// Shown for a repository with no workflow at this ref. Writers get a link to the new-file page
/// opened in the workflow directory.
fn get_started(model: &ActionsPage) -> Element {
    let can_write = model
        .repo_chrome
        .as_ref()
        .is_some_and(|repo| repo.can_write);
    let new_file = repository_base(&model.base_path)
        .filter(|_| can_write)
        .map(|base| {
            repos::ref_path::file_url(base, "new", &model.dispatch_branch, WORKFLOW_DIRECTORY)
        });
    rsx! {
        section { class: "actions-get-started", aria_labelledby: "actions-get-started-title",
            h2 { id: "actions-get-started-title", "Get started with Actions" }
            p { "Add a workflow file to " code { "{WORKFLOW_DIRECTORY}/" } ". It runs on the events it lists, such as a push." }
            pre { code { "{WORKFLOW_EXAMPLE}" } }
            if let Some(href) = new_file { NavLink { class: "btn btn-primary", href, "Create a workflow file" } }
        }
    }
}

/// One workflow run as a row of a run list: the Actions page's list, or the Checks tab of a pull
/// request. The caller gives the name and the link, because each list opens a run on its own page.
pub(in crate::webui::pages::repos) fn run_row(
    run: &Run,
    workflow: String,
    href: String,
    show_repository: bool,
) -> Element {
    rsx! {
        NavLink { class: "actions-run-row", href: href,
            aria_label: Some(format!("{workflow}, {}, {}, run {}", logs::normalize_status(&run.status), run.branch, run.id)),
            {status_chip(&run.status)}
            div { class: "actions-run-description",
                span { class: "actions-run-title", "{workflow}" }
                div { class: "actions-run-subtitle",
                    span { "{run.event}" }
                    span { title: run.id.clone(), "Run #{short_id(&run.id, 8)}" }
                    span { class: "sr-only", "Run ID {run.id}" }
                    if show_repository { span { "{run.repository}" } }
                    if let Some(sha) = &run.head_sha { code { title: sha.clone(), "{short_sha(sha)}" } }
                }
            }
            span { class: "actions-branch-badge", title: run.branch.clone(), {icon("branches")} "{run.branch}" }
            div { class: "actions-run-time",
                span { class: "actions-time-line", {icon("calendar")} LiveTime { value: logs::rfc3339(run.created_at), title: Some(logs::timestamp(run.created_at)) } }
                span { class: "actions-time-line actions-run-duration", {icon("duration")} "{run_duration(run)}" }
            }
        }
    }
}

#[component]
pub(in crate::webui::pages) fn ActionsView(model: ActionsPage) -> Element {
    let active = model.selected.as_ref().is_some_and(|selected| {
        matches!(
            logs::normalize_status(&selected.run.status),
            "running" | "queued"
        )
    });
    // Keep the run's log editor/search/details mounted across status transitions.
    // Only the timer component is removed on terminal status, which cancels its
    // use_future task without discarding the user's unsubmitted local controls.
    rsx! {
        if active {
            super::polling::ActiveRunPoll { key: "{model.selected.as_ref().unwrap().run.id}", run_id: model.selected.as_ref().unwrap().run.id.clone() }
        }
        {actions_page(model)}
    }
}

// Quoted native default attributes intentionally bypass Dioxus's volatile
// controlled-input setters. Unrelated model polls must not reset unsent GET or
// dispatch drafts; the signal-owned log controls below remain controlled.
pub fn actions_page(mut model: ActionsPage) -> Element {
    let selected = model.selected.take().map(std::rc::Rc::new);
    let filtered: Vec<_> = model
        .runs
        .iter()
        .filter(|run| matches_filters(run, &model.query))
        .collect();
    let heading = model
        .workflows
        .iter()
        .find(|workflow| workflow.path == model.value("workflow"))
        .map(|workflow| workflow.name.as_str())
        .unwrap_or(if model.value("workflow").is_empty() {
            "All workflows"
        } else {
            "Workflow runs"
        });
    // The events a workflow can run on, so the filter has choices before any run exists.
    let mut events: std::collections::BTreeSet<&str> =
        ["pull_request", "push", "workflow_dispatch"]
            .into_iter()
            .collect();
    events.extend(model.runs.iter().map(|run| run.event.as_str()));
    if !model.value("event").is_empty() {
        events.insert(model.value("event"));
    }
    // A repository with no workflows shows the heading and the get-started panel only.
    let needs_setup = model.workflows.is_empty()
        && model.repository.is_some()
        && model.workflow_warning.is_none();
    // The Run workflow menu only appears when a workflow can be started by hand.
    let can_run =
        model.can_dispatch && model.workflows.iter().any(|workflow| workflow.dispatchable);
    // The fetched runs are capped at run_limit, so only a full page can have older runs behind it.
    let body = rsx! {
        div { class: "actions-page", "aria-label": model.title.clone(),
            if let Some(warning) = &model.workflow_warning {
                p { role: "alert", "{warning}" }
            }
            if let Some(selected) = &selected {
                {selected_run(&model, selected.clone())}
            } else {
                div { class: "actions-list-layout",
                    aside { class: "actions-sidebar", "aria-label": "Workflows",
                        h2 { class: "actions-sidebar-heading", {icon("actions")} "Actions" }
                        NavLink { class: if model.value("workflow").is_empty() { "actions-nav-item is-selected" } else { "actions-nav-item" }, href: model.url(&[("workflow", ""), ("before", "")]), {workflow_icon()} "All workflows" }
                        div { class: "actions-sidebar-divider" }
                        for workflow in &model.workflows {
                            NavLink { class: if model.value("workflow") == workflow.path { "actions-nav-item is-selected" } else { "actions-nav-item" }, href: model.url(&[("workflow", &workflow.path), ("before", "")]), "{workflow.name}" }
                        }
                    }
                    section { class: "actions-content", "aria-label": "Actions runs",
                        header { class: "actions-list-heading",
                            div {
                                h1 { "{heading}" }
                                p { class: "actions-heading-subtitle", "Workflow runs" }
                            }
                            if !needs_setup {
                                div { class: "actions-heading-controls",
                                    if model.base_path == "/ci" {
                                        {manual_dispatch_form(&model.base_path, &model.csrf_token, &DispatchDraft::default())}
                                    } else if can_run { {dispatch_form(&model)} }
                                    RefreshLink { href: model.url(&[]) }
                                }
                            }
                        }
                        if needs_setup {
                            {get_started(&model)}
                        } else {
                            div { class: "actions-run-toolbar",
                                span { class: "actions-run-count", "{filtered.len()} workflow run" if filtered.len() != 1 { "s" } }
                                form { id: "actions-run-filters", method: "get", action: model.base_path.clone(), class: "actions-filters",
                                    {preserved_inputs(&model, &["q", "workflow", "event", "status", "branch", "actor", "before", "job", "view", "workflow_job"])}
                                    label { class: "actions-search-field",
                                        span { class: "sr-only", "Search runs" }
                                        {icon("search")}
                                        input { class: "actions-search", r#type: "search", name: "q", "value": model.value("q"), placeholder: "Filter workflow runs" }
                                    }
                                    label { class: "actions-filter-field",
                                        span { class: "sr-only", "Workflow" }
                                        DraftSelect { class: "actions-filter", name: "workflow", initial_value: model.value("workflow").to_owned(), options: workflow_filter_options(&model) }
                                    }
                                    label { class: "actions-filter-field",
                                        span { class: "sr-only", "Event" }
                                        DraftSelect { class: "actions-filter", name: "event", initial_value: model.value("event").to_owned(),
                                            options: std::iter::once(SelectOption::new("", "Any event"))
                                                .chain(events.iter().map(|event| SelectOption::new(*event, *event)))
                                                .collect::<Vec<_>>() }
                                    }
                                    label { class: "actions-filter-field",
                                        span { class: "sr-only", "Status" }
                                        DraftSelect { class: "actions-filter", name: "status", initial_value: model.value("status").to_owned(), options: status_filter_options(&model) }
                                    }
                                    label { class: "actions-filter-field",
                                        span { class: "sr-only", "Filter by branch" }
                                        input { class: "actions-filter actions-branch-filter", name: "branch", "value": model.value("branch"), placeholder: "Branch" }
                                    }
                                    label { class: "actions-filter-field",
                                        span { class: "sr-only", "Actor" }
                                        input { class: "actions-filter", name: "actor", "value": model.value("actor"), placeholder: "Actor" }
                                    }
                                    button { class: "actions-button", r#type: "submit", "Filter runs" }
                                }
                            }
                            section { class: "actions-runs", "aria-label": "Workflow runs",
                                if filtered.is_empty() {
                                    div { class: "actions-empty",
                                        {workflow_icon()}
                                        h2 { if model.runs.is_empty() { "No workflow runs yet" } else { "No matching workflow runs" } }
                                        p { if model.runs.is_empty() { "Workflow runs will appear here when they are queued." } else { "Try changing your search or filters." } }
                                    }
                                } else {
                                    ul {
                                        for run in filtered {
                                            li { key: "{run.id}",
                                                {run_row(run, display_workflow(&model, run), model.url(&[("job", &run.id), ("view", "logs"), ("workflow_job", "")]), model.base_path == "/ci")}
                                            }
                                        }
                                    }
                                }
                            }
                            if model.next_before.is_some() || !model.value("before").is_empty() {
                                div { class: "actions-list-footer",
                                    if !model.value("before").is_empty() { NavLink { class: "actions-history-link", href: model.url(&[("before", "")]), "Newest runs" } }
                                    if let Some(before) = model.next_before { NavLink { class: "actions-history-link", href: model.url(&[("before", &before.to_string())]), if ["q", "workflow", "event", "status", "branch", "actor"].iter().any(|key| !model.value(key).is_empty()) { "Older matching runs" } else { "Older runs" } } }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    repository_frame(model.repo_chrome.as_ref(), body)
}

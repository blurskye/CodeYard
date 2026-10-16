use super::{
    dispatch::{dispatch_form, manual_dispatch_form, WorkflowInput},
    logs,
    run::{display_workflow, run_duration, selected_run, short_id},
    DispatchDraft, Run, SelectedRun,
};
use crate::webui::{
    components::{DraftSelect, SelectOption},
    encode_component,
    navigation::{NavLink, RefreshLink},
    pages::repositories::{self, RepoChrome},
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
    pub run_limit: usize,
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
            } else {
                circle { cx: "12", cy: "12", r: "10" }
                path { d: "M12 6v6h4" }
            }
        }
    }
}

// Original Actions/repository SVGs, with presentation only: no session/API state.
pub(super) fn icon(kind: &str) -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "1.5", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true",
            match kind {
                "code" => rsx! { path { d: "m18 16 4-4-4-4M6 8l-4 4 4 4m8.5-12-5 16" } },
                "issues" => rsx! { circle { cx: "12", cy: "12", r: "10" } circle { cx: "12", cy: "12", r: "1" } },
                "pulls" => rsx! { circle { cx: "18", cy: "18", r: "3" } circle { cx: "6", cy: "6", r: "3" } path { d: "M13 6h3a2 2 0 0 1 2 2v7M6 9v12" } },
                "actions" => rsx! { circle { cx: "12", cy: "12", r: "9" } path { d: "m10 8 6 4-6 4z" } },
                "commits" => rsx! { path { d: "M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8M3 3v5h5M12 7v5l4 2" } },
                "branches" => rsx! { path { d: "M6 3v12M18 9a9 9 0 0 1-9 9" } circle { cx: "18", cy: "6", r: "3" } circle { cx: "6", cy: "18", r: "3" } },
                "tags" => rsx! { path { d: "M12.586 2.586A2 2 0 0 0 11.172 2H4a2 2 0 0 0-2 2v7.172a2 2 0 0 0 .586 1.414l8.704 8.704a2.426 2.426 0 0 0 3.42 0l6.58-6.58a2.426 2.426 0 0 0 0-3.42z" } circle { cx: "7.5", cy: "7.5", r: ".5", fill: "currentColor" } },
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
            div { class: "actions-repository", {repositories::chrome(repo, body)} }
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

/// Shown for a repository with no workflow at this ref. Writers get a link to the new-file page
/// opened in the workflow directory.
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

fn get_started(model: &ActionsPage) -> Element {
    let can_write = model
        .repo_chrome
        .as_ref()
        .is_some_and(|repo| repo.can_write);
    let new_file = repository_base(&model.base_path)
        .filter(|_| can_write)
        .map(|base| {
            repositories::ref_path::file_url(
                base,
                "new",
                &model.dispatch_branch,
                WORKFLOW_DIRECTORY,
            )
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

#[component]
pub(super) fn ActionsView(model: ActionsPage) -> Element {
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
        if active { ActiveRunPoll {} }
        {actions_page(model)}
    }
}

#[component]
fn ActiveRunPoll() -> Element {
    use_future(move || async move {
        #[cfg(target_arch = "wasm32")]
        loop {
            gloo_timers::future::TimeoutFuture::new(2000).await;
            crate::webui::navigation::refresh_page();
        }
    });
    rsx! {}
}

// Quoted native default attributes intentionally bypass Dioxus's volatile
// controlled-input setters. Unrelated model polls must not reset unsent GET or
// dispatch drafts; the signal-owned log controls below remain controlled.
pub fn actions_page(model: ActionsPage) -> Element {
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
    let needs_setup = model.workflows.is_empty() && model.repository.is_some();
    // The fetched runs are capped at run_limit, so only a full page can have older runs behind it.
    let body = rsx! {
        div { class: "actions-page", "aria-label": model.title.clone(),
            if let Some(selected) = &model.selected {
                {selected_run(&model, selected)}
            } else {
                div { class: "actions-list-layout",
                    aside { class: "actions-sidebar", "aria-label": "Workflows",
                        h2 { class: "actions-sidebar-heading", {icon("actions")} "Actions" }
                        NavLink { class: if model.value("workflow").is_empty() { "actions-nav-item is-selected" } else { "actions-nav-item" }, href: model.url(&[("workflow", "")]), {workflow_icon()} "All workflows" }
                        div { class: "actions-sidebar-divider" }
                        for workflow in &model.workflows {
                            NavLink { class: if model.value("workflow") == workflow.path { "actions-nav-item is-selected" } else { "actions-nav-item" }, href: model.url(&[("workflow", &workflow.path)]), "{workflow.name}" }
                        }
                        if model.workflows.is_empty() {
                            p { class: "actions-sidebar-note", "Add a workflow to start automating this repository." }
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
                                    if model.base_path != "/ci" {
                                        form { method: "get", action: model.base_path.clone(), class: "actions-version-form", aria_label: "Workflow version",
                                            {preserved_inputs(&model, &["ref", "job", "view", "workflow_job"])}
                                            label { class: "actions-version-field",
                                                span { "Run on" }
                                                input { class: "actions-filter actions-branch-filter", name: "ref", aria_label: "Run on", "value": model.dispatch_branch.clone(), required: true, maxlength: "255" }
                                            }
                                            button { r#type: "submit", class: "btn btn-sm", "View workflows" }
                                        }
                                    }
                                    div { class: "actions-search-row",
                                        label { class: "actions-search-field",
                                            span { class: "actions-sr-only", "Search runs" }
                                            {icon("search")}
                                            input { class: "actions-search", r#type: "search", name: "q", form: "actions-run-filters", "value": model.value("q"), placeholder: "Filter workflow runs" }
                                        }
                                    }
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
                                    {preserved_inputs(&model, &["q", "workflow", "event", "status", "branch", "job", "view", "workflow_job"])}
                                    label { class: "actions-filter-field",
                                        span { class: "actions-sr-only", "Workflow" }
                                        DraftSelect { class: "actions-filter", name: "workflow", initial_value: model.value("workflow").to_owned(), options: workflow_filter_options(&model) }
                                    }
                                    label { class: "actions-filter-field",
                                        span { class: "actions-sr-only", "Event" }
                                        DraftSelect { class: "actions-filter", name: "event", initial_value: model.value("event").to_owned(),
                                            options: std::iter::once(SelectOption::new("", "Any event"))
                                                .chain(events.iter().map(|event| SelectOption::new(*event, *event)))
                                                .collect::<Vec<_>>() }
                                    }
                                    label { class: "actions-filter-field",
                                        span { class: "actions-sr-only", "Status" }
                                        DraftSelect { class: "actions-filter", name: "status", initial_value: model.value("status").to_owned(), options: status_filter_options(&model) }
                                    }
                                    label { class: "actions-filter-field actions-filter-branch",
                                        span { class: "actions-sr-only", "Filter by branch" }
                                        input { class: "actions-filter actions-branch-filter", name: "branch", "value": model.value("branch"), placeholder: "Branch" }
                                    }
                                    button { class: "actions-button", r#type: "submit", "Filter runs" }
                                }
                                if model.base_path == "/ci" {
                                    {manual_dispatch_form(&model.base_path, &model.csrf_token, &DispatchDraft::default())}
                                } else if model.can_dispatch { {dispatch_form(&model)} }
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
                                                NavLink { class: "actions-run-row", href: model.url(&[("job", &run.id), ("view", "logs"), ("workflow_job", "")]),
                                                    aria_label: Some(format!("{}, {}, {}, run {}", display_workflow(&model, run), logs::normalize_status(&run.status), run.branch, run.id)),
                                                    span { class: "actions-run-icon", title: status_label(&run.status), {status_icon(&run.status)} span { class: "actions-sr-only", "{status_label(&run.status)}" } }
                                                    div { class: "actions-run-description",
                                                        span { class: "actions-run-title", "{display_workflow(&model, run)}" }
                                                        div { class: "actions-run-subtitle",
                                                            span { "{run.event}" }
                                                            span { title: run.id.clone(), "Run #{short_id(&run.id, 8)}" }
                                                            span { class: "actions-sr-only", "Run ID {run.id}" }
                                                            if model.base_path == "/ci" { span { "{run.repository}" } }
                                                            if let Some(sha) = &run.head_sha { code { title: sha.clone(), "{short_id(sha, 7)}" } }
                                                        }
                                                    }
                                                    span { class: "actions-branch-badge", title: run.branch.clone(), {icon("branches")} "{run.branch}" }
                                                    div { class: "actions-run-time",
                                                        span { class: "actions-time-line", {icon("calendar")} time { "{logs::timestamp(run.created_at)}" } }
                                                        span { class: "actions-time-line actions-run-duration", {icon("duration")} "{run_duration(run)}" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            if model.runs.len() >= model.run_limit {
                                div { class: "actions-list-footer",
                                    p { class: "actions-list-footnote", "Showing the latest {model.run_limit} runs." }
                                    if model.run_limit < 200 { NavLink { class: "actions-history-link", href: model.url(&[("limit", "200")]), "Show more runs" } }
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

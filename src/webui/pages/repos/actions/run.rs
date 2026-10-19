use super::{
    logs::{self, LogLine, LogOutput},
    runs::{icon, repository_base, status_chip, workflow_icon, workflow_name},
    ActionsPage,
};
use crate::webui::{
    encode_component,
    navigation::{NavLink, RefreshLink},
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub repository: String,
    pub status: String,
    pub workflow_path: String,
    pub commit_status_context: String,
    pub branch: String,
    pub event: String,
    pub pipeline: String,
    pub mode: String,
    pub head_sha: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub error_message: Option<String>,
    /// The inputs of a manual run, by name; empty for other events.
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Deployment {
    pub environment: String,
    pub status: String,
    pub approved_at: Option<i64>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SelectedRun {
    pub run: Run,
    pub logs: Vec<LogLine>,
    pub deployment: Option<Deployment>,
    pub fetched_lines: usize,
    #[serde(default)]
    pub logs_truncated: bool,
}

pub(super) fn display_workflow(model: &ActionsPage, run: &Run) -> String {
    model
        .workflows
        .iter()
        .find(|workflow| {
            run.workflow_path
                .split(',')
                .any(|path| path.trim() == workflow.path)
        })
        .map(|workflow| workflow.name.clone())
        .unwrap_or_else(|| workflow_name(&run.workflow_path))
}

pub(super) fn short_id(value: &str, length: usize) -> String {
    value.chars().take(length).collect()
}

/// The timing note beside a run's status: how long it took, or what it is waiting for. The
/// status itself is shown separately, so this never repeats it.
pub(super) fn run_duration(run: &Run) -> String {
    match (run.started_at, run.completed_at) {
        (Some(start), Some(end)) => logs::duration(end - start),
        _ => match (logs::normalize_status(&run.status), run.started_at) {
            ("queued", _) => "Waiting for a runner".into(),
            ("running", Some(start)) => format!("Started {}", logs::relative_time(start)),
            _ => "Not recorded".into(),
        },
    }
}

/// Links the workflow file as the run's commit defines it, not the branch's current
/// copy. A run without a recorded commit gets no link.
pub(super) fn workflow_file_url(model: &ActionsPage, run: &Run) -> Option<String> {
    let base = repository_base(&model.base_path)?;
    let sha = run.head_sha.as_deref()?;
    let path = run.workflow_path.split(',').next()?.trim();
    if !path.starts_with(".github/workflows/")
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return None;
    }
    let path = path
        .split('/')
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/");
    Some(format!("{base}/blob/{}/{path}", encode_component(sha)))
}

/// The commit's short id. It links to the commit page when the repository has one.
fn commit_link(model: &ActionsPage, sha: &str) -> Element {
    match repository_base(&model.base_path) {
        Some(base) => rsx! {
            NavLink { class: "actions-history-link", href: format!("{base}/commit/{}", encode_component(sha)), code { title: sha.to_owned(), "{short_id(sha, 7)}" } }
        },
        None => rsx! { code { title: sha.to_owned(), "{short_id(sha, 7)}" } },
    }
}

/// A button that posts an action on this run through its own native form. The form carries the
/// CSRF token and works without scripting; the server checks write access again.
fn run_form(model: &ActionsPage, run_id: &str, action: &str, label: &str) -> Element {
    rsx! {
        form { method: "post", action: format!("{}/runs/{run_id}/{action}", model.base_path), class: "actions-run-form",
            {crate::webui::pages::repos::issues::csrf(&model.csrf_token)}
            button { r#type: "submit", class: "actions-button", "{label}" }
        }
    }
}

pub(super) fn selected_run(model: &ActionsPage, selected: &SelectedRun) -> Element {
    let run = &selected.run;
    let jobs = logs::project_logs(&selected.logs, &run.status);
    let show_logs = model.value("view") != "summary";
    let log_key = format!(
        "{}:{}:{}",
        run.id,
        model.value("log_q"),
        model.value("wrap")
    );
    let error_lines = selected
        .logs
        .iter()
        .filter(|line| matches!(line.level.to_ascii_lowercase().as_str(), "error" | "fatal"))
        .count();
    let cancellable = matches!(logs::normalize_status(&run.status), "queued" | "running");
    // A cancelled run stores a message too, but it did not fail.
    let run_failed =
        run.error_message.is_some() && logs::normalize_status(&run.status) != "cancelled";
    rsx! {
        header { class: "actions-run-heading",
            NavLink { class: "actions-back", href: model.url(&[("job", ""), ("view", ""), ("workflow_job", ""), ("log_q", "")]), "← Workflow runs" }
            div { class: "actions-run-heading-row",
                h1 { "{display_workflow(model, run)}"
                    span { class: "actions-run-number", title: run.id.clone(), " #{short_id(&run.id, 8)}" }
                    span { class: "actions-sr-only", "Run ID {run.id}" }
                }
                div { class: "actions-run-heading-actions",
                    if model.can_dispatch && cancellable { {run_form(model, &run.id, "cancel", "Cancel run")} }
                    if model.can_dispatch && !cancellable && run.head_sha.is_some() && run.event != "pull_request" { {run_form(model, &run.id, "rerun", "Re-run")} }
                    RefreshLink { href: model.url(&[]) }
                }
            }
            div { class: "actions-run-meta",
                {status_chip(&run.status)}
                span { "Triggered via {run.event}" }
                span { class: "actions-heading-branch", "{run.branch}" }
                if let Some(sha) = &run.head_sha { {commit_link(model, sha)} }
                span { "{run_duration(run)}" }
            }
        }
        div { class: "actions-detail-layout",
            aside { class: "actions-sidebar", "aria-label": "Run navigation",
                div { class: "actions-sidebar-card",
                    NavLink { class: if show_logs { "actions-nav-item" } else { "actions-nav-item is-selected" }, href: model.url(&[("view", "summary")]), {icon("summary")} "Summary" }
                    div { class: "actions-sidebar-divider" }
                    h2 { class: "actions-sidebar-label", "Jobs" }
                    for (index, job) in jobs.iter().enumerate() {
                        NavLink { class: if show_logs && (model.value("workflow_job") == job.name || (model.value("workflow_job").is_empty() && index == 0)) { "actions-nav-item is-selected" } else { "actions-nav-item" }, href: model.url(&[("view", "logs"), ("workflow_job", &job.name)]),
                            {workflow_icon()} span { class: "actions-job-name", "{job.name}" }
                        }
                    }
                    if jobs.is_empty() { p { class: "actions-sidebar-note", "Jobs will appear when the runner starts." } }
                    div { class: "actions-sidebar-divider" }
                    h2 { class: "actions-sidebar-label", "Details" }
                    a { class: "actions-nav-item actions-muted-link", href: "#run-metadata", {icon("usage")} "Run details" }
                    if let Some(href) = workflow_file_url(model, run) {
                        NavLink { class: "actions-nav-item actions-muted-link", href, {icon("file")} "Workflow file" }
                    }
                }
            }
            div { class: "actions-detail-main",
                if error_lines > 0 || run_failed {
                    details { class: "actions-annotations", open: true,
                        summary {
                            span { h2 { "Annotations" }
                                span { class: "actions-annotation-count",
                                    if error_lines > 0 { "{error_lines} error line" if error_lines != 1 { "s" } }
                                    else { "Runner reported an error" }
                                }
                            }
                            span { class: "actions-disclosure-label", "Details" }
                        }
                        if run_failed { p { role: "alert", "This run reported an error. Open the failed step for details." } }
                    }
                }
                if show_logs {
                    LogOutput { key: "{log_key}", model: model.clone(), selected: selected.clone() }
                } else {
                    section { class: "actions-summary-card", id: "run-summary", "aria-label": "Run summary",
                        dl { class: "actions-summary-grid",
                            div { dt { "Triggered" } dd { "via {run.event}" }
                                dd { class: "actions-summary-secondary", span { title: logs::timestamp(run.created_at), "{logs::relative_time(run.created_at)}" } }
                            }
                            div { dt { "Status" } dd { {status_chip(&run.status)} } }
                            div { dt { "Total duration" } dd { "{run_duration(run)}" } }
                            div { dt { "Commit" } dd {
                                if let Some(sha) = &run.head_sha { {commit_link(model, sha)} } else { span { class: "actions-summary-secondary", "Not recorded" } }
                            } }
                        }
                    }
                    section { class: "actions-workflow-card", "aria-label": "Workflow jobs",
                        header {
                            h2 { "{run.workflow_path}" }
                            p { "on {run.event} · {run.branch}" }
                        }
                        div { class: "actions-workflow-jobs",
                            if jobs.is_empty() { p { class: "actions-workflow-empty", "Jobs will appear when the runner starts." } }
                            for job in &jobs {
                                NavLink { class: "actions-workflow-job", href: model.url(&[("view", "logs"), ("workflow_job", &job.name)]),
                                    span { class: "actions-workflow-job-name", {workflow_icon()} "{job.name}" }
                                    span { class: "actions-workflow-job-count", "{job.steps.len()} step" if job.steps.len() != 1 { "s" } }
                                }
                            }
                        }
                    }
                }
                if let Some(deployment) = &selected.deployment {
                    section { class: "actions-deployment-card", "aria-label": "Deployment",
                        h2 { "Deployment" }
                        dl { class: "actions-metadata-grid",
                            div { dt { "Environment" } dd { "{deployment.environment}" } }
                            div { dt { "Status" } dd { "{deployment.status}" } }
                            if let Some(approved) = deployment.approved_at { div { dt { "Approved" } dd { time { "{logs::timestamp(approved)}" } } } }
                            if let Some(started) = deployment.started_at { div { dt { "Started" } dd { time { "{logs::timestamp(started)}" } } } }
                            if let Some(completed) = deployment.completed_at { div { dt { "Completed" } dd { time { "{logs::timestamp(completed)}" } } } }
                        }
                    }
                }
                details { class: "actions-metadata",
                    summary { {icon("usage")} "Run details" }
                    // A fragment inside details lets native navigation reveal its ancestor.
                    dl { class: "actions-metadata-grid", id: "run-metadata",
                        div { dt { "Run ID" } dd { code { "{run.id}" } } }
                        div { dt { "Repository" } dd { "{run.repository}" } }
                        div { dt { "Workflow" } dd { "{run.workflow_path}" } }
                        div { dt { "Commit status" } dd { code { "{run.commit_status_context}" } } }
                        div { dt { "Event" } dd { "{run.event}" } }
                        div { dt { "Branch" } dd { "{run.branch}" } }
                        div { dt { "Runner / pipeline" } dd { "{run.mode} / {run.pipeline}" } }
                        div { dt { "Commit" } dd { code { {run.head_sha.as_deref().unwrap_or("Not recorded")} } } }
                        div { dt { "Created" } dd { "{logs::timestamp(run.created_at)}" } }
                        div { dt { "Started" } dd { {run.started_at.map(logs::timestamp).unwrap_or_else(|| "Not started".into())} } }
                        div { dt { "Completed" } dd { {run.completed_at.map(logs::timestamp).unwrap_or_else(|| "Not completed".into())} } }
                        div { dt { "Duration" } dd { "{run_duration(run)}" } }
                        for (name, value) in &run.inputs {
                            div { dt { "Input {name}" } dd { code { "{value}" } } }
                        }
                    }
                }
            }
        }
    }
}

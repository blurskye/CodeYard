use super::{
    logs::{self, LogLine, LogOutput},
    runs::{icon, repository_base, status_icon, status_label, workflow_icon, workflow_name},
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
            ("running", Some(start)) => format!("Started {}", logs::timestamp(start)),
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
    rsx! {
        header { class: "actions-run-heading",
            NavLink { class: "actions-back", href: model.url(&[("job", ""), ("view", ""), ("workflow_job", ""), ("log_q", "")]), "← Workflow runs" }
            div { class: "actions-run-heading-row",
                div { class: "actions-run-heading-title",
                    {status_icon(&run.status)}
                    h1 { "{display_workflow(model, run)}"
                        span { class: "actions-run-number", title: run.id.clone(), " #{short_id(&run.id, 8)}" }
                        span { class: "actions-sr-only", "Run ID {run.id}" }
                    }
                }
                RefreshLink { href: model.url(&[]) }
            }
            p { span { class: "actions-run-state", "{status_label(&run.status)}" } " · {run_duration(run)} · " span { class: "actions-heading-branch", "{run.branch}" } }
        }
        div { class: "actions-detail-layout",
            aside { class: "actions-sidebar", "aria-label": "Run navigation",
                div { class: "actions-sidebar-card",
                    NavLink { class: if show_logs { "actions-nav-item" } else { "actions-nav-item is-selected" }, href: model.url(&[("view", "summary")]), {icon("summary")} "Summary" }
                    div { class: "actions-sidebar-divider" }
                    h2 { class: "actions-sidebar-label", "Jobs" }
                    NavLink { class: if show_logs && model.value("workflow_job").is_empty() { "actions-nav-item is-selected" } else { "actions-nav-item" }, href: model.url(&[("view", "logs"), ("workflow_job", "")]), {workflow_icon()} "All jobs" }
                    for job in &jobs {
                        NavLink { class: if show_logs && model.value("workflow_job") == job.name { "actions-nav-item is-selected" } else { "actions-nav-item" }, href: model.url(&[("view", "logs"), ("workflow_job", &job.name)]),
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
                if error_lines > 0 || run.error_message.is_some() {
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
                        p { role: "alert", "This run reported an error. Open the failed step for details." }
                    }
                }
                if show_logs {
                    LogOutput { key: "{log_key}", model: model.clone(), selected: selected.clone() }
                } else {
                    section { class: "actions-summary-card", id: "run-summary", "aria-label": "Run summary",
                        dl { class: "actions-summary-grid",
                            div { dt { "Triggered" } dd { "via {run.event}" }
                                dd { class: "actions-summary-secondary", time { "{logs::timestamp(run.created_at)}" } }
                            }
                            div { dt { "Status" } dd { class: "actions-summary-status", {status_icon(&run.status)} "{status_label(&run.status)}" } }
                            div { dt { "Total duration" } dd { "{run_duration(run)}" } }
                            div { dt { "Commit" } dd {
                                if let Some(sha) = &run.head_sha {
                                    if let Some(base) = repository_base(&model.base_path) {
                                        NavLink { class: "actions-history-link", href: format!("{base}/commit/{}", encode_component(sha)), code { title: sha.clone(), "{short_id(sha, 7)}" } }
                                    } else { code { title: sha.clone(), "{short_id(sha, 7)}" } }
                                } else { span { class: "actions-summary-secondary", "Not recorded" } }
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

use super::{
    logs::{self, LogLine, LogOutput},
    runs::{icon, repository_base, status_chip, workflow_icon, workflow_name},
    ActionsPage,
};
use crate::webui::{
    components::{relative_time, short_sha, LiveTime, Markdown},
    encode_component,
    navigation::{NavLink, RefreshLink},
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    #[serde(default)]
    pub approval_required: bool,
    #[serde(default)]
    pub approval_pull: Option<String>,
    #[serde(default)]
    pub concurrency_groups: Vec<String>,
    #[serde(default)]
    pub waiting_for_concurrency: bool,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    pub id: String,
    pub api_id: i64,
    pub api_attempt: i32,
    pub repository: String,
    pub status: String,
    pub workflow_path: String,
    pub commit_status_context: String,
    pub branch: String,
    pub event: String,
    pub pipeline: String,
    pub mode: String,
    #[serde(default)]
    pub engine: Option<String>,
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
pub struct Artifact {
    pub can_delete: bool,
    pub id: i64,
    pub name: String,
    pub size: i64,
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
pub struct RunProgress {
    pub status: String,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SelectedRun {
    pub run: Run,
    #[serde(default)]
    pub summaries: Vec<Summary>,
    pub logs: Vec<LogLine>,
    pub deployment: Option<Deployment>,
    pub fetched_lines: usize,
    #[serde(default)]
    pub logs_truncated: bool,
    /// The jobs and steps the run's workflow file lists, shown before their logs arrive.
    #[serde(default)]
    pub planned: Vec<PlannedJob>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub job: String,
    pub number: i32,
    pub markdown: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlannedJob {
    #[serde(default)]
    pub workflow: String,
    pub name: String,
    pub steps: Vec<String>,
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
    if run.status == "queued" && run.approval_required {
        return "Waiting for fork workflow approval".into();
    }
    if run.status == "queued" && run.waiting_for_concurrency {
        return "Waiting for concurrency group".into();
    }
    match (run.started_at, run.completed_at) {
        (Some(start), Some(end)) => logs::duration(end - start),
        _ => match (logs::normalize_status(&run.status), run.started_at) {
            ("queued", _) => "Waiting for a runner".into(),
            ("running", Some(start)) => format!("Started {}", relative_time(start)),
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
    Some(crate::webui::pages::repos::ref_path::file_url(
        base, "blob", sha, path,
    ))
}

/// The commit's short id. It links to the commit page when the repository has one.
fn commit_link(model: &ActionsPage, sha: &str) -> Element {
    match repository_base(&model.base_path) {
        Some(base) => rsx! {
            NavLink { class: "actions-history-link", href: format!("{base}/commit/{}", encode_component(sha)), code { title: sha.to_owned(), "{short_sha(sha)}" } }
        },
        None => rsx! { code { title: sha.to_owned(), "{short_sha(sha)}" } },
    }
}

/// A button that posts an action on this run through its own native form. The form carries the
/// CSRF token and works without scripting; the server checks write access again.
fn run_form(model: &ActionsPage, run_id: &str, action: &str, label: &str) -> Element {
    rsx! {
        form { method: "post", action: format!("{}/runs/{run_id}/{action}", model.base_path), class: "actions-run-form",
            {crate::webui::pages::repos::chrome::csrf(&model.csrf_token)}
            button { r#type: "submit", class: "actions-button", "{label}" }
        }
    }
}

pub(super) fn selected_run(model: &ActionsPage, selected: std::rc::Rc<SelectedRun>) -> Element {
    debug_assert!(model.selected.is_none());
    rsx! { SelectedRunView { model: model.clone(), selected } }
}

#[component]
fn SelectedRunView(model: ActionsPage, selected: std::rc::Rc<SelectedRun>) -> Element {
    let projection = use_memo(use_reactive((&selected,), |(selected,)| {
        let mut jobs = logs::project_logs(&selected.logs, &selected.run.status);
        logs::add_planned_steps(&mut jobs, &selected.planned, &selected.run.status);
        std::rc::Rc::new(jobs)
    }));
    let jobs = projection();
    let chosen = logs::selected_job(&jobs, model.value("workflow_job"));
    let model = &model;
    let selected = &selected;
    let run = &selected.run;
    let engine = run.engine.as_deref().unwrap_or(if run.status == "queued" {
        "Not started"
    } else {
        "act"
    });
    let show_logs = model.value("view") != "summary";
    let log_key = format!(
        "{}:{}:{}",
        run.id,
        model.value("log_q"),
        model.value("wrap")
    );
    let annotations: Vec<_> = selected
        .logs
        .iter()
        .filter_map(|line| logs::annotation(&line.line).map(|issue| (line.id, issue)))
        .collect();
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
                    span { class: "sr-only", "Run ID {run.id}" }
                }
                div { class: "actions-run-heading-actions",
                    if model.can_cancel && cancellable { {run_form(model, &run.id, "cancel", "Cancel run")} }
                    if model.can_manage_runs && !cancellable && run.head_sha.is_some() { {run_form(model, &run.id, "rerun", "Re-run")} }
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
                        NavLink { class: if show_logs && chosen == Some(index) { "actions-nav-item is-selected" } else { "actions-nav-item" }, href: model.url(&[("view", "logs"), ("workflow_job", &job.key)]),
                            {workflow_icon()} span { class: "actions-job-name", "{job.name}" if !job.stage.is_empty() { small { " · {job.stage}" } } }
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
                if !annotations.is_empty() || error_lines > 0 || run_failed {
                    details { class: "actions-annotations", open: true,
                        summary {
                            span { h2 { "Annotations" }
                                span { class: "actions-annotation-count",
                                    if !annotations.is_empty() { "{annotations.len()} annotation" if annotations.len() != 1 { "s" } }
                                    else if error_lines > 0 { "{error_lines} error line" if error_lines != 1 { "s" } }
                                    else { "Runner reported an error" }
                                }
                            }
                            span { class: "actions-disclosure-label", "Details" }
                        }
                        if run_failed { p { role: "alert", "{run.error_message.as_deref().unwrap_or_default()}" } }
                        for (id, issue) in &annotations {
                            div { class: "actions-annotation",
                                strong { "{issue.severity}" if !issue.title.is_empty() { ": {issue.title}" } }
                                if !issue.file.is_empty() { p { "{issue.file}" if !issue.line.is_empty() { ":{issue.line}" } if !issue.column.is_empty() { ":{issue.column}" } } }
                                p { "{issue.message}" }
                                NavLink { class: "actions-history-link", href: {
                                    let job = jobs.iter().find(|job| job.steps.iter().any(|step| step.lines.iter().any(|line| line.id == *id))).map_or("", |job| job.key.as_str());
                                    format!("{}#log-{id}", model.url(&[("view", "logs"), ("workflow_job", job), ("log_q", "")]))
                                }, "View log line" }
                            }
                        }
                    }
                }
                if show_logs {
                    LogOutput { key: "{log_key}", model: model.clone(), run: run.clone(), logs_empty: selected.logs.is_empty(), fetched_lines: selected.fetched_lines, logs_truncated: selected.logs_truncated, jobs: jobs.clone() }
                } else {
                    section { class: "actions-summary-card", id: "run-summary", "aria-label": "Run summary",
                        dl { class: "actions-summary-grid",
                            div { dt { "Triggered" } dd { "via {run.event}" }
                                dd { class: "actions-summary-secondary", LiveTime { value: logs::rfc3339(run.created_at), title: Some(logs::timestamp(run.created_at)) } }
                            }
                            div { dt { "Status" } dd { {status_chip(&run.status)} } }
                            div { dt { "Total duration" } dd { "{run_duration(run)}" } }
                            div { dt { "Commit" } dd {
                                if let Some(sha) = &run.head_sha { {commit_link(model, sha)} } else { span { class: "actions-summary-secondary", "Not recorded" } }
                            } }
                        }
                    }
                    for summary in &selected.summaries {
                        section { class: "actions-summary-card", aria_label: "Step summary",
                            h2 { "{summary.job} · step {summary.number}" }
                            Markdown { content: summary.markdown.clone() }
                        }
                    }
                    section { class: "actions-workflow-card", "aria-label": "Workflow jobs",
                        header {
                            h2 { "{run.workflow_path}" }
                            p { "on {run.event} · {run.branch}" }
                        }
                        div { class: "actions-workflow-jobs",
                            if jobs.is_empty() { p { class: "actions-workflow-empty", "Jobs will appear when the runner starts." } }
                            for job in jobs.iter() {
                                NavLink { class: "actions-workflow-job", href: model.url(&[("view", "logs"), ("workflow_job", &job.key)]),
                                    span { class: "actions-workflow-job-name", {workflow_icon()} "{job.name}" if !job.stage.is_empty() { small { " · {job.stage}" } } }
                                    span { class: "actions-workflow-job-count", "{job.steps.len()} step" if job.steps.len() != 1 { "s" } }
                                }
                            }
                        }
                    }
                }
                if !run.artifacts.is_empty() {
                    section { class: "actions-summary-card", aria_label: "Artifacts",
                        h2 { "Artifacts" }
                        for artifact in &run.artifacts {
                            div { class: "actions-run-heading-row",
                                a { class: "actions-history-link", style: "overflow-wrap:anywhere;min-width:0", href: format!("{}/runs/{}/artifacts/{}/download", model.base_path, run.id, artifact.id), "{artifact.name}.zip" }
                                span { "{artifact.size} bytes" }
                                if artifact.can_delete {
                                    form { method: "post", action: format!("{}/runs/{}/artifacts/{}/delete", model.base_path, run.id, artifact.id),
                                        {crate::webui::pages::repos::chrome::csrf(&model.csrf_token)}
                                        button { r#type: "submit", class: "actions-button", "Delete" }
                                    }
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
                        if run.status == "queued" && run.approval_required {
                            if let Some(url) = &run.approval_pull {
                                div { dt { "Approval" } dd {
                                    a { href: "{url}", "Review fork workflows on the pull request" }
                                } }
                            }
                        }
                        div { dt { "Run ID" } dd { code { "{run.id}" } } }
                        div { dt { "Repository" } dd { "{run.repository}" } }
                        div { dt { "Workflow" } dd { "{run.workflow_path}" } }
                        for group in &run.concurrency_groups {
                            div { dt { "Concurrency group" } dd { style: "overflow-wrap:anywhere;min-width:0", code { "{group}" } } }
                        }
                        div { dt { "Commit status" } dd { code { "{run.commit_status_context}" } } }
                        div { dt { "Event" } dd { "{run.event}" } }
                        div { dt { "Branch" } dd { "{run.branch}" } }
                        div { dt { "Engine" } dd { "{engine}" } }
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

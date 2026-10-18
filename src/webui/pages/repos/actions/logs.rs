//! Pure projection of persisted act output. A missing marker never proves success.
use super::{
    run::run_duration,
    runs::{icon, preserved_inputs, status_icon, status_label},
    ActionsPage, SelectedRun,
};
use crate::webui::{
    components::{current_time, relative_age, DraftSelect, SelectOption},
    navigation::NavLink,
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    pub id: i64,
    pub stage: String,
    pub line: String,
    pub level: String,
    pub timestamp: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    pub status: String,
    pub duration: Option<String>,
    pub lines: Vec<LogLine>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowJob {
    pub name: String,
    pub steps: Vec<Step>,
}

pub fn normalize_status(status: &str) -> &'static str {
    match status.to_ascii_lowercase().as_str() {
        "success" | "completed" => "success",
        "failed" | "error" => "failed",
        "running" | "in_progress" => "running",
        "queued" | "pending" => "queued",
        "cancelled" | "canceled" => "cancelled",
        _ => "unknown",
    }
}

pub fn duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

pub fn timestamp(value: i64) -> String {
    chrono::DateTime::from_timestamp(value, 0)
        .map(|value| value.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "Unknown time".to_string())
}

/// "5 minutes ago", "3 days ago". The exact time goes in the title beside it.
pub fn relative_time(value: i64) -> String {
    chrono::DateTime::from_timestamp(value, 0)
        .map(|then| relative_age(then, current_time()))
        .unwrap_or_else(|| "Unknown time".to_string())
}

/// Reused from the reference's pure act parser, not its browser store/runtime.
pub fn act_job_name(line: &str) -> Option<String> {
    let inner = line.strip_prefix('[')?;
    let token = &inner[..inner.find(']')?];
    let separator = token.rfind('/')?;
    if separator == 0 || separator + 1 >= token.len() {
        return None;
    }
    let name = token[separator + 1..].trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn step_name(name: &str) -> String {
    let trimmed = name.trim_start();
    for prefix in ["Main ", "Post ", "main ", "post ", "MAIN ", "POST "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.trim().to_string();
        }
    }
    trimmed.trim().to_string()
}

fn act_payload(line: &str) -> Option<&str> {
    let inner = line.strip_prefix('[')?;
    Some(inner[inner.find(']')? + 1..].trim_start())
}

pub fn act_step_start(line: &str) -> Option<String> {
    let rest = act_payload(line)?.strip_prefix('⭐')?.trim_start();
    let rest = rest.strip_prefix("Run")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let name = step_name(rest);
    (!name.is_empty()).then_some(name)
}

pub fn act_step_outcome(line: &str) -> Option<(String, &'static str, Option<String>)> {
    let rest = act_payload(line)?;
    let (status, rest) = if let Some(rest) = rest.strip_prefix('✅') {
        ("success", rest.trim_start().strip_prefix("Success")?)
    } else {
        (
            "failed",
            rest.strip_prefix('❌')?
                .trim_start()
                .strip_prefix("Failure")?,
        )
    };
    let remainder = rest.trim_start().strip_prefix('-')?.trim_end();
    let (name, timing) = if remainder.ends_with(']') {
        match remainder.rfind('[') {
            Some(open) => (
                &remainder[..open],
                Some(remainder[open + 1..remainder.len() - 1].trim().to_string()),
            ),
            None => (remainder, None),
        }
    } else {
        (remainder, None)
    };
    let name = step_name(name);
    if name.is_empty() {
        return None;
    }
    Some((name, status, timing.filter(|value| !value.is_empty())))
}

/// act ends a job with a flag line, "🏁  Job succeeded" or "🏁  Job failed". It gets the same
/// status icon as a step outcome.
pub fn act_job_result(line: &str) -> Option<(&'static str, &'static str)> {
    match act_payload(line)?.trim_start_matches('🏁').trim() {
        "Job succeeded" => Some(("success", "Job succeeded")),
        "Job failed" => Some(("failed", "Job failed")),
        _ => None,
    }
}

pub fn display_log_text(line: &str) -> &str {
    if act_job_name(line).is_some() {
        if let Some((_, text)) = line.split_once(']') {
            let text = text.trim_start();
            return text.strip_prefix("| ").unwrap_or(text);
        }
    }
    line
}

fn new_step(name: String) -> Step {
    Step {
        name,
        status: "unknown".to_string(),
        duration: None,
        lines: Vec::new(),
    }
}

fn steps(lines: &[LogLine], run_status: &str) -> Vec<Step> {
    let mut result: Vec<Step> = Vec::new();
    let mut active: Option<usize> = None;
    for line in lines {
        if let Some(name) = act_step_start(&line.line) {
            let mut step = new_step(name);
            step.status = "running".to_string();
            result.push(step);
            active = Some(result.len() - 1);
        }
        let outcome = act_step_outcome(&line.line);
        // Late/mismatched outcomes must not mark an unrelated active step successful.
        let target = outcome
            .as_ref()
            .and_then(|(name, _, _)| {
                result
                    .iter()
                    .rposition(|step| step.name == *name && step.status == "running")
            })
            .or(active);
        let index = target.unwrap_or_else(|| {
            let name = if line.stage.is_empty() {
                "Runner"
            } else {
                &line.stage
            };
            if let Some(index) = result
                .iter()
                .rposition(|step| step.name == name && step.status == "unknown")
            {
                index
            } else {
                result.push(new_step(name.to_string()));
                result.len() - 1
            }
        });
        result[index].lines.push(line.clone());
        if matches!(line.level.to_ascii_lowercase().as_str(), "error" | "fatal") {
            result[index].status = "failed".to_string();
        }
        if let Some((name, status, timing)) = outcome {
            if result[index].name == name {
                result[index].status = status.to_string();
                result[index].duration = timing;
                if active == Some(index) {
                    active = None;
                }
            }
        }
    }
    let normalized = normalize_status(run_status);
    let last_active = result.iter().rposition(|step| step.status == "running");
    for (index, step) in result.iter_mut().enumerate() {
        if step.status == "running" && (normalized != "running" || last_active != Some(index)) {
            step.status = "unknown".to_string();
        }
    }
    result
}

/// Preserve every delivered persisted line, including prelude/trailer and malformed text.
pub fn project_logs(lines: &[LogLine], run_status: &str) -> Vec<WorkflowJob> {
    let mut buckets: Vec<(String, Vec<LogLine>)> = Vec::new();
    let mut current = "Runner".to_string();
    for line in lines {
        if let Some(name) = act_job_name(&line.line) {
            current = name;
        }
        if let Some((_, bucket)) = buckets.iter_mut().find(|(name, _)| name == &current) {
            bucket.push(line.clone());
        } else {
            buckets.push((current.clone(), vec![line.clone()]));
        }
    }
    buckets
        .into_iter()
        .map(|(name, lines)| WorkflowJob {
            name,
            steps: steps(&lines, run_status),
        })
        .collect()
}

pub fn search_steps(steps: &[Step], query: &str) -> Vec<Step> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return steps.to_vec();
    }
    steps
        .iter()
        .filter_map(|step| {
            let mut visible = step.clone();
            if !step.name.to_lowercase().contains(&query) {
                visible.lines.retain(|line| {
                    format!("{} {} {}", line.stage, line.level, line.line)
                        .to_lowercase()
                        .contains(&query)
                });
            }
            (!visible.lines.is_empty()).then_some(visible)
        })
        .collect()
}

#[component]
pub(super) fn LogOutput(model: ActionsPage, selected: SelectedRun) -> Element {
    let initial_query = model.value("log_q").to_string();
    let initial_wrap = model.value("wrap") != "0";
    let mut query = use_signal(move || initial_query);
    let mut wrap = use_signal(move || initial_wrap);
    // One job at a time: the job the sidebar chose, or the first job when none is chosen.
    let jobs = project_logs(&selected.logs, &selected.run.status);
    let visible_jobs: Vec<_> = jobs
        .iter()
        .enumerate()
        .filter(|(index, job)| {
            if model.value("workflow_job").is_empty() {
                *index == 0
            } else {
                model.value("workflow_job") == job.name
            }
        })
        .collect();
    let heading = match visible_jobs.first() {
        Some((_, job)) => job.name.clone(),
        None => "Logs".to_string(),
    };
    rsx! {
        section { id: "job-logs", class: "actions-log-panel", "aria-label": "Workflow logs",
            header { class: "actions-log-heading",
                div {
                    h2 { "{heading}" }
                    p { "{status_label(&selected.run.status)} · {run_duration(&selected.run)}" }
                }
                form { method: "get", action: model.base_path.clone(), class: "actions-log-controls",
                    {preserved_inputs(&model, &["log_q", "wrap"])}
                    label { class: "actions-search-field actions-log-search",
                        span { class: "actions-sr-only", "Search logs" }
                        {icon("search")}
                        input { class: "actions-search", name: "log_q", r#type: "search", placeholder: "Search logs", value: query(), maxlength: "300", oninput: move |event| query.set(event.value()) }
                    }
                    label { class: "actions-wrap-control", title: "Wrap log lines",
                        {icon("wrap")} span { class: "actions-sr-only", "Wrap lines" }
                        DraftSelect { name: "wrap", initial_value: String::from(if wrap() { "1" } else { "0" }),
                            options: vec![SelectOption::new("1", "Wrap"), SelectOption::new("0", "No wrap")],
                            on_value: move |value: String| wrap.set(value != "0") }
                    }
                }
            }
            div { class: "actions-log-jobs",
                if selected.logs.is_empty() {
                    div { class: "actions-log-empty",
                        match normalize_status(&selected.run.status) {
                            "queued" => rsx! {
                                p { "Waiting for the runner to start; logs appear here as the job runs." }
                                p { class: "actions-log-meta", "Queued " span { title: timestamp(selected.run.created_at), "{relative_time(selected.run.created_at)}" } }
                            },
                            "running" => rsx! { p { "Waiting for the first log lines; logs appear here as the job runs." } },
                            _ => rsx! { p { "No logs were recorded for this run." } },
                        }
                    }
                }
                for (job_index, job) in &visible_jobs {
                    section { class: "actions-log-job",
                        div { class: "actions-step-list", {render_steps(*job_index, job, &query(), wrap())} }
                    }
                }
                if !model.value("workflow_job").is_empty() && visible_jobs.is_empty() && !selected.logs.is_empty() {
                    p { class: "actions-log-empty", "This job is not available in the recent logs." }
                }
            }
            footer { class: "actions-log-footer",
                if selected.logs_truncated {
                    p { class: "actions-log-limit", role: "status", "Recent logs were shortened to fit this page. Run status continues to update." }
                } else if selected.fetched_lines >= model.log_limit {
                    p { class: "actions-log-limit", role: "status", "Only the most recent {model.log_limit} lines are shown." }
                }
                if selected.logs_truncated || selected.fetched_lines >= model.log_limit {
                    a { class: "actions-history-link", href: format!("/api/ci/jobs/{}/history", selected.run.id), "Full log history (paginated JSON)" }
                }
                details { class: "actions-help",
                    summary { "About these logs" }
                    p { "Logs update automatically while a run is active. You can also refresh the page at any time." }
                    if model.log_limit < 5000 { NavLink { class: "actions-history-link", href: model.url(&[("log_limit", "5000")]), "Show more log lines" } }
                }
            }
        }
    }
}

/// A step start or outcome line from act shows its status as an icon, not an emoji. The raw
/// line stays in the title, so copying it still gives the original text.
fn log_text(line: &str) -> Element {
    if let Some(name) = act_step_start(line) {
        return rsx! {
            span { class: "actions-log-text actions-log-marker", title: line.to_owned(), {icon("play")} span { "{name}" } }
        };
    }
    if let Some((name, status, timing)) = act_step_outcome(line) {
        return rsx! {
            span { class: "actions-log-text actions-log-marker", title: line.to_owned(),
                {status_icon(status)}
                span { class: "actions-sr-only", "{status_label(status)}: " }
                span { "{name}" }
                if let Some(timing) = timing { span { class: "actions-log-timing", "{timing}" } }
            }
        };
    }
    if let Some((status, text)) = act_job_result(line) {
        return rsx! {
            span { class: "actions-log-text actions-log-marker", title: line.to_owned(),
                {status_icon(status)}
                span { "{text}" }
            }
        };
    }
    rsx! { span { class: "actions-log-text", "{display_log_text(line)}" } }
}

fn render_steps(job_index: usize, job: &WorkflowJob, query: &str, wrap: bool) -> Element {
    let visible = search_steps(&job.steps, query);
    rsx! {
        if visible.is_empty() { p { class: "actions-log-empty", "No log lines match your search." } }
        for (step_index, step) in visible.iter().enumerate() {
            details { class: "actions-step", open: !query.is_empty() || matches!(step.status.as_str(), "failed" | "running"),
                summary { class: "actions-step-heading", id: format!("stage-{job_index}-{step_index}"),
                    span { class: "actions-step-name",
                        {status_icon(&step.status)}
                        span { "{step.name}" }
                        if matches!(step.status.as_str(), "unknown" | "cancelled" | "queued") {
                            span { class: "actions-step-outcome", "{status_label(&step.status)}" }
                        } else { span { class: "actions-sr-only", "{step.status}" } }
                    }
                    span { class: "actions-step-duration",
                        if let Some(duration) = &step.duration { "{duration}" }
                        if !query.is_empty() { " · {step.lines.len()} matching lines" }
                    }
                }
                div { class: if wrap { "actions-log-lines wrap-lines" } else { "actions-log-lines" },
                    for line in &step.lines {
                        div { id: format!("log-{}", line.id), class: match line.level.to_ascii_lowercase().as_str() {
                            "error" | "fatal" => "actions-log-line is-error",
                            "warn" | "warning" => "actions-log-line is-warning",
                            _ => "actions-log-line",
                        },
                            a { class: "actions-log-line-number", href: format!("#log-{}", line.id), title: timestamp(line.timestamp), "{line.id}" }
                            {log_text(&line.line)}
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn log(id: i64, line: &str) -> LogLine {
        LogLine {
            id,
            stage: "ci".into(),
            line: line.into(),
            level: "info".into(),
            timestamp: id,
        }
    }
    #[test]
    fn real_act_markers_project_steps_without_losing_output() {
        let lines = vec![
            log(1, "Preparing runner"),
            log(2, "[CI/test] ⭐ Run Main checkout"),
            log(3, "[CI/test] | Checking out revision abc"),
            log(4, "[CI/test] ✅ Success - Main checkout [15.6ms]"),
            log(5, "[CI/test] ⭐ Run Main test"),
            log(6, "[CI/test] | test failed: expected 2, got 1"),
            log(7, "[CI/test] ❌ Failure - Main test [2.1s]"),
            log(8, "[CI/test] Job failed"),
        ];
        let jobs = project_logs(&lines, "failed");
        assert_eq!(jobs[0].name, "Runner");
        assert_eq!(jobs[1].steps[0].name, "checkout");
        assert_eq!(jobs[1].steps[0].status, "success");
        assert_eq!(jobs[1].steps[1].status, "failed");
        let mut ids: Vec<_> = jobs
            .iter()
            .flat_map(|job| job.steps.iter())
            .flat_map(|step| step.lines.iter().map(|line| line.id))
            .collect();
        ids.sort();
        assert_eq!(ids, (1..=8).collect::<Vec<_>>());
    }
    #[test]
    fn job_flag_lines_read_as_the_job_result() {
        assert_eq!(
            act_job_result("[CI/test] 🏁  Job failed"),
            Some(("failed", "Job failed"))
        );
        assert_eq!(
            act_job_result("[CI/test] 🏁  Job succeeded"),
            Some(("success", "Job succeeded"))
        );
        assert_eq!(act_job_result("[CI/test] | Job failed"), None);
    }

    #[test]
    fn missing_outcomes_and_mismatched_markers_do_not_invent_success() {
        let jobs = project_logs(
            &[
                log(1, "[CI/test] ⭐ Run Main test"),
                log(2, "[CI/test] ✅ Success - Main unrelated [1s]"),
            ],
            "success",
        );
        assert_eq!(jobs[0].steps[0].status, "unknown");
        assert_eq!(
            project_logs(&[log(1, "output only")], "success")[0].steps[0].status,
            "unknown"
        );
        assert!(project_logs(&[], "success").is_empty());
    }
    #[test]
    fn malformed_unicode_and_search_preserve_text() {
        for text in [
            "[",
            "[x/]",
            "[a/b] ⭐ Runaway",
            "[a/b] ✅ Success -",
            "λ <script>alert(1)</script>",
        ] {
            assert!(act_step_start(text).is_none());
            assert!(act_step_outcome(text).is_none());
            let jobs = project_logs(&[log(1, text)], "running");
            assert_eq!(jobs[0].steps[0].lines[0].line, text);
        }
        let jobs = project_logs(&[log(1, "needle λ"), log(2, "other")], "running");
        assert_eq!(search_steps(&jobs[0].steps, "NEEDLE")[0].lines.len(), 1);
    }
    #[test]
    fn only_last_unfinished_step_is_active() {
        let jobs = project_logs(
            &[
                log(1, "[a/b] ⭐ Run Main first"),
                log(2, "[a/b] ⭐ Run Main second"),
            ],
            "running",
        );
        assert_eq!(jobs[0].steps[0].status, "unknown");
        assert_eq!(jobs[0].steps[1].status, "running");
    }
}

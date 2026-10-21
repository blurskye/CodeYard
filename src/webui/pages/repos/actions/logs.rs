//! Pure projection of persisted act output. A missing marker never proves success.
use super::{
    run::run_duration,
    runs::{icon, preserved_inputs, status_icon, status_label},
    ActionsPage, PlannedJob, Run,
};
use crate::webui::{
    components::{DraftSelect, LiveTime, SelectOption},
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
    pub key: String,
    pub stage: String,
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
        "skipped" => "skipped",
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

/// A Unix time in seconds as RFC 3339 text, for a live age. Empty when it is out of range.
pub fn rfc3339(value: i64) -> String {
    chrono::DateTime::from_timestamp(value, 0)
        .map(|value| value.to_rfc3339())
        .unwrap_or_default()
}

pub fn timestamp(value: i64) -> String {
    chrono::DateTime::from_timestamp(value, 0)
        .map(|value| value.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "Unknown time".to_string())
}

/// The job name in the bracketed prefix of an `act` output line, such as `[CI/build]`.
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

/// The time at the end of a line, "[140.28ms]". act puts the time of a step with a multi-line
/// command after its last line.
fn trailing_timing(line: &str) -> Option<String> {
    let inner = line.trim_end().strip_suffix(']')?;
    let value = inner[inner.rfind('[')? + 1..].trim();
    let digits = value.chars().next()?.is_ascii_digit();
    let unit = |ch: char| ch.is_ascii_digit() || matches!(ch, '.' | 'µ' | 'n' | 'm' | 's' | 'h');
    (digits && value.chars().all(unit)).then(|| value.to_owned())
}

/// A step time as act prints it ("35.195068ms", "1.234567s") rounded for reading.
pub fn short_duration(timing: &str) -> String {
    if let Some(milliseconds) = timing
        .strip_suffix("ms")
        .and_then(|v| v.parse::<f64>().ok())
    {
        return format!("{milliseconds:.0}ms");
    }
    if let Some(seconds) = timing.strip_suffix('s').and_then(|v| v.parse::<f64>().ok()) {
        return format!("{seconds:.1}s");
    }
    timing.to_owned()
}

/// Whether `line` is the next line of a multi-line command that act prints again after the step's
/// end marker, with the step's time after the last one.
fn repeats_command_line(line: &str, expected: &str) -> bool {
    line.strip_prefix(expected)
        .is_some_and(|rest| rest.trim().is_empty() || trailing_timing(rest).is_some())
}

fn steps(lines: &[&LogLine], run_status: &str) -> Vec<Step> {
    let mut result: Vec<Step> = Vec::new();
    let mut active: Option<usize> = None;
    // act prints the lines of a multi-line command without its `[job]` prefix, once after the
    // step's start marker and again after its end marker. These keep them in their step.
    let mut command_lines: std::collections::HashMap<usize, Vec<String>> =
        std::collections::HashMap::new();
    let mut command_over = false;
    let mut closed: Option<usize> = None;
    let mut repeated = 0;
    for line in lines {
        let prefixed = act_payload(&line.line).is_some();
        let start = act_step_start(&line.line);
        if let Some(name) = start.clone() {
            let mut step = new_step(name);
            step.status = "running".to_string();
            result.push(step);
            active = Some(result.len() - 1);
            command_over = false;
        }
        let outcome = act_step_outcome(&line.line);
        let repeat = !prefixed
            && closed.is_some_and(|index| {
                match command_lines.get(&index).and_then(|l| l.get(repeated)) {
                    Some(expected) => repeats_command_line(&line.line, expected),
                    None => trailing_timing(&line.line)
                        .is_some_and(|_| line.line.trim_start().starts_with('[')),
                }
            });
        // Late/mismatched outcomes must not mark an unrelated active step successful. An outcome
        // whose start line is not in the lines (a log window starts later) gets a step of its own.
        let matched = outcome.as_ref().and_then(|(name, _, _)| {
            // An error line marks the step failed before its end marker arrives.
            active
                .filter(|&index| result[index].name == *name)
                .or_else(|| {
                    result
                        .iter()
                        .rposition(|step| step.name == *name && step.status == "running")
                })
        });
        if let (Some((name, _, _)), None) = (&outcome, matched) {
            result.push(new_step(name.clone()));
        }
        let target = matched
            .or(outcome.as_ref().and(result.len().checked_sub(1)))
            .or(active)
            .or(if repeat { closed } else { None })
            // The "Job succeeded" flag closes the last step.
            .or(if act_job_result(&line.line).is_some() {
                result.len().checked_sub(1)
            } else {
                None
            });
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
        result[index].lines.push((*line).clone());
        if matches!(line.level.to_ascii_lowercase().as_str(), "error" | "fatal") {
            result[index].status = "failed".to_string();
        }
        if repeat {
            repeated += 1;
            if let Some(timing) = trailing_timing(&line.line) {
                result[index].duration = Some(timing);
            }
        } else if prefixed {
            closed = None;
            if start.is_none() {
                command_over = true;
            }
        } else if active == Some(index) && !command_over {
            command_lines
                .entry(index)
                .or_default()
                .push(line.line.clone());
        }
        if let Some((name, status, timing)) = outcome {
            if result[index].name == name {
                result[index].status = status.to_string();
                result[index].duration = timing;
                closed = Some(index);
                repeated = 0;
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
    let mut buckets: Vec<(String, String, String, Vec<&LogLine>)> = Vec::new();
    let mut current = std::collections::HashMap::<&str, String>::new();
    for line in lines {
        let identity = current
            .entry(&line.stage)
            .or_insert_with(|| "Runner".into());
        if act_job_name(&line.line).is_some() {
            *identity = line.line[1..line.line.find(']').expect("job prefix")]
                .trim()
                .to_owned();
        }
        if let Some((_, _, _, bucket)) = buckets
            .iter_mut()
            .find(|(stage, job, _, _)| stage == &line.stage && job == identity)
        {
            bucket.push(line);
        } else {
            let name = act_job_name(&line.line).unwrap_or_else(|| "Runner".into());
            buckets.push((line.stage.clone(), identity.clone(), name, vec![line]));
        }
        // act's unprefixed output after a job's end flag is the runner's, not that job's.
        if act_job_result(&line.line).is_some() {
            *identity = "Runner".into();
        }
    }
    buckets
        .into_iter()
        .map(|(stage, identity, name, lines)| WorkflowJob {
            key: format!("{}:{stage}:{identity}", stage.len()),
            stage,
            name,
            steps: steps(&lines, run_status),
        })
        .collect()
}

/// Adds the steps the workflow file lists but the logs have not reached: pending while the run
/// is active, skipped once it has ended. A planned job with no logs yet is listed the same way.
pub fn add_planned_steps(jobs: &mut Vec<WorkflowJob>, planned: &[PlannedJob], run_status: &str) {
    let missing = if matches!(normalize_status(run_status), "queued" | "running") {
        "pending"
    } else {
        "skipped"
    };
    for plan in planned.iter().filter(|plan| !plan.steps.is_empty()) {
        let names = std::iter::once("Set up job")
            .chain(plan.steps.iter().map(|step| step.as_str()))
            .chain(std::iter::once("Complete job"));
        let job = match jobs.iter().position(|job| job.name == plan.name) {
            Some(index) => &mut jobs[index],
            None => {
                jobs.push(WorkflowJob {
                    key: format!("planned:{}", plan.name),
                    stage: String::new(),
                    name: plan.name.clone(),
                    steps: Vec::new(),
                });
                jobs.last_mut().expect("pushed job")
            }
        };
        // Each missing step goes after the step the file lists before it.
        let mut at = 0;
        for name in names {
            if let Some(index) = job.steps.iter().position(|step| step.name == name) {
                at = at.max(index + 1);
            } else {
                let mut step = new_step(name.to_owned());
                step.status = missing.to_owned();
                job.steps.insert(at, step);
                at += 1;
            }
        }
    }
}

pub fn selected_job(jobs: &[WorkflowJob], selection: &str) -> Option<usize> {
    if selection.is_empty() {
        // The runner's own lines come first; open the first workflow job instead when there is one.
        return jobs
            .iter()
            .position(|job| job.name != "Runner")
            .or((!jobs.is_empty()).then_some(0));
    }
    jobs.iter()
        .position(|job| job.key == selection)
        .or_else(|| {
            let mut matches = jobs
                .iter()
                .enumerate()
                .filter(|(_, job)| job.name == selection);
            let (index, _) = matches.next()?;
            matches.next().is_none().then_some(index)
        })
}

pub fn search_steps<'a>(steps: &'a [Step], query: &str) -> Vec<(&'a Step, Vec<&'a LogLine>)> {
    let query = query.trim().to_lowercase();
    steps
        .iter()
        .filter_map(|step| {
            let all = query.is_empty() || step.name.to_lowercase().contains(&query);
            let lines: Vec<_> = step
                .lines
                .iter()
                .filter(|line| {
                    all || format!("{} {} {}", line.stage, line.level, line.line)
                        .to_lowercase()
                        .contains(&query)
                })
                .collect();
            (!lines.is_empty() || query.is_empty()).then_some((step, lines))
        })
        .collect()
}

#[component]
pub(super) fn LogOutput(
    model: ActionsPage,
    run: Run,
    logs_empty: bool,
    fetched_lines: usize,
    logs_truncated: bool,
    jobs: std::rc::Rc<Vec<WorkflowJob>>,
) -> Element {
    let initial_query = model.value("log_q").to_string();
    let initial_wrap = model.value("wrap") != "0";
    let mut query = use_signal(move || initial_query);
    let mut wrap = use_signal(move || initial_wrap);
    // One job at a time: the job the sidebar chose, or the first job when none is chosen.
    let job = selected_job(&jobs, model.value("workflow_job")).and_then(|index| jobs.get(index));
    let heading = job.map_or_else(|| "Logs".to_string(), |job| job.name.clone());
    rsx! {
        section { id: "job-logs", class: "actions-log-panel", "aria-label": "Workflow logs",
            header { class: "actions-log-heading",
                div {
                    h2 { "{heading}" }
                    p { "{status_label(&run.status)} · {run_duration(&run)}" }
                }
                form { method: "get", action: model.base_path.clone(), class: "actions-log-controls",
                    {preserved_inputs(&model, &["log_q", "wrap"])}
                    label { class: "actions-search-field actions-log-search",
                        span { class: "sr-only", "Search logs" }
                        {icon("search")}
                        crate::webui::components::DraftInput { class: "actions-search", name: "log_q", input_type: "search", placeholder: "Search logs", initial_value: query(), maxlength: Some(300), on_value: move |value| query.set(value) }
                    }
                    label { class: "actions-wrap-control", title: "Wrap log lines",
                        {icon("wrap")} span { class: "sr-only", "Wrap lines" }
                        DraftSelect { name: "wrap", initial_value: String::from(if wrap() { "1" } else { "0" }),
                            options: vec![SelectOption::new("1", "Wrap"), SelectOption::new("0", "No wrap")],
                            on_value: move |value: String| wrap.set(value != "0") }
                    }
                }
            }
            div { class: "actions-log-jobs",
                if logs_empty {
                    div { class: "actions-log-empty",
                        match normalize_status(&run.status) {
                            "queued" => rsx! {
                                p { "Waiting for the runner to start; logs appear here as the job runs." }
                                p { class: "actions-log-meta", "Queued " LiveTime { value: rfc3339(run.created_at), title: Some(timestamp(run.created_at)) } }
                            },
                            "running" => rsx! { p { "Waiting for the first log lines; logs appear here as the job runs." } },
                            _ => rsx! { p { "No logs were recorded for this run." } },
                        }
                    }
                }
                if let Some(job) = job {
                    section { class: "actions-log-job",
                        div { class: "actions-step-list", {render_steps(job, &query(), wrap())} }
                    }
                }
                if !model.value("workflow_job").is_empty() && job.is_none() && !logs_empty {
                    p { class: "actions-log-empty", "This job is not available in the recent logs." }
                }
            }
            footer { class: "actions-log-footer",
                if logs_truncated {
                    p { class: "actions-log-limit", role: "status", "Recent logs were shortened to fit this page. Run status continues to update." }
                } else if fetched_lines >= model.log_limit {
                    p { class: "actions-log-limit", role: "status", "Only the most recent {model.log_limit} lines are shown." }
                }
                if logs_truncated || fetched_lines >= model.log_limit {
                    a { class: "actions-history-link", href: format!("/api/ci/jobs/{}/history", run.id), "Full log history (paginated JSON)" }
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
                span { class: "sr-only", "{status_label(status)}: " }
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

fn render_steps(job: &WorkflowJob, query: &str, wrap: bool) -> Element {
    let visible = search_steps(&job.steps, query);
    rsx! {
        if visible.is_empty() { p { class: "actions-log-empty", "No log lines match your search." } }
        for (step, lines) in &visible {
            details { class: "actions-step", open: !query.is_empty() || matches!(step.status.as_str(), "failed" | "running"),
                summary { class: "actions-step-heading",
                    span { class: "actions-step-name",
                        {status_icon(&step.status)}
                        span { "{step.name}" }
                        if matches!(step.status.as_str(), "unknown" | "cancelled" | "queued") {
                            span { class: "actions-step-outcome", "{status_label(&step.status)}" }
                        } else { span { class: "sr-only", "{step.status}" } }
                    }
                    span { class: "actions-step-duration",
                        if let Some(duration) = &step.duration { "{short_duration(duration)}" }
                        if !query.is_empty() { " · {lines.len()} matching lines" }
                    }
                }
                div { class: if wrap { "actions-log-lines wrap-lines" } else { "actions-log-lines" },
                    for line in lines {
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
    fn job_identity_includes_workflow_stage_and_the_full_act_prefix() {
        let mut lines = vec![
            log(1, "[CI/test] ⭐ Run Main first"),
            log(2, "[CI/test] ⭐ Run Main second"),
            log(3, "[Deploy/test] ⭐ Run Main third"),
        ];
        lines[0].stage = "ci.yml".into();
        lines[1].stage = "deploy-image.yml".into();
        lines[2].stage = "ci.yml".into();
        let jobs = project_logs(&lines, "running");
        assert_eq!(jobs.len(), 3);
        assert!(jobs.iter().all(|job| job.name == "test"));
        for (index, job) in jobs.iter().enumerate() {
            assert_eq!(selected_job(&jobs, &job.key), Some(index));
            assert_eq!(job.steps[0].lines[0].id, index as i64 + 1);
        }
        assert_eq!(selected_job(&jobs, "test"), None);
    }

    #[test]
    fn the_first_workflow_job_opens_before_the_runner_lines() {
        let lines = vec![
            log(1, "Using docker host"),
            log(2, "[CI/test] ⭐ Run Main checkout"),
            log(3, "[CI/test] ✅ Success - Main checkout [15.6ms]"),
        ];
        let jobs = project_logs(&lines, "success");
        assert_eq!(jobs[0].name, "Runner");
        assert_eq!(selected_job(&jobs, ""), Some(1));
        assert_eq!(selected_job(&jobs[..1], ""), Some(0));
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
    fn a_multi_line_command_keeps_its_lines_and_time_in_its_step() {
        let lines = vec![
            log(1, "[CI/build] ⭐ Run Main echo a"),
            log(2, "test -f README.md"),
            log(3, "[CI/build]   | a"),
            log(4, "[CI/build]   ✅  Success - Main echo a"),
            log(5, "test -f README.md"),
            log(6, " [140.282868ms]"),
            log(7, "[CI/build] ⭐ Run Complete job"),
            log(8, "[CI/build]   ✅  Success - Complete job"),
        ];
        let jobs = project_logs(&lines, "success");
        let steps = &jobs[0].steps;
        assert_eq!(steps.len(), 2, "{steps:?}");
        assert_eq!(steps[0].lines.len(), 6);
        assert_eq!(steps[0].duration.as_deref(), Some("140.282868ms"));
        assert_eq!(short_duration("140.282868ms"), "140ms");
        assert_eq!(short_duration("1.234567s"), "1.2s");
        assert_eq!(short_duration("1m2.5s"), "1m2.5s");
    }
    #[test]
    fn the_job_flag_stays_with_the_last_step() {
        let lines = vec![
            log(1, "[CI/build] ⭐ Run Complete job"),
            log(2, "[CI/build]   ✅  Success - Complete job"),
            log(3, "[CI/build] 🏁  Job succeeded"),
            log(4, "level=info msg=A newer version of 'act' is available"),
        ];
        let jobs = project_logs(&lines, "success");
        assert_eq!(jobs[0].steps.len(), 1, "{:?}", jobs[0].steps);
        assert_eq!(jobs[0].steps[0].lines.len(), 3);
        // act's own trailer after the flag is runner output, not a step of the job.
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[1].name, "Runner");
        assert_eq!(jobs[1].steps[0].lines[0].id, 4);
    }
    #[test]
    fn planned_steps_show_before_their_logs_and_are_skipped_after_the_run() {
        let plan = vec![
            PlannedJob {
                name: "build".into(),
                steps: vec!["checkout".into(), "test".into()],
            },
            PlannedJob {
                name: "deploy".into(),
                steps: vec!["ship".into()],
            },
        ];
        let lines = vec![
            log(1, "[CI/build] ⭐ Run Set up job"),
            log(2, "[CI/build]   ✅  Success - Set up job"),
            log(3, "[CI/build] ⭐ Run Main checkout"),
        ];
        let mut jobs = project_logs(&lines, "running");
        add_planned_steps(&mut jobs, &plan, "running");
        let steps: Vec<_> = jobs[0]
            .steps
            .iter()
            .map(|step| (step.name.as_str(), step.status.as_str()))
            .collect();
        assert_eq!(
            steps,
            [
                ("Set up job", "success"),
                ("checkout", "running"),
                ("test", "pending"),
                ("Complete job", "pending"),
            ]
        );
        assert_eq!(jobs[1].name, "deploy");
        assert!(jobs[1].steps.iter().all(|step| step.status == "pending"));
        assert_eq!(selected_job(&jobs, ""), Some(0));

        let mut finished = project_logs(&lines, "failed");
        add_planned_steps(&mut finished, &plan, "failed");
        assert_eq!(finished[0].steps[2].status, "skipped");
        assert_eq!(normalize_status("skipped"), "skipped");
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
        let found = search_steps(&jobs[0].steps, "NEEDLE");
        assert_eq!(found[0].1.len(), 1);
        assert!(std::ptr::eq(found[0].0, &jobs[0].steps[0]));
        assert!(std::ptr::eq(found[0].1[0], &jobs[0].steps[0].lines[0]));
        assert!(search_steps(&jobs[0].steps, "absent").is_empty());
    }
    #[test]
    fn an_outcome_without_its_start_line_keeps_its_status_and_time() {
        let jobs = project_logs(
            &[
                log(1, "[CI/test] | late output"),
                log(2, "[CI/test] ❌ Failure - Main test [2.1s]"),
            ],
            "failed",
        );
        let step = jobs[0]
            .steps
            .iter()
            .find(|step| step.name == "test")
            .unwrap();
        assert_eq!(step.status, "failed");
        assert_eq!(step.duration.as_deref(), Some("2.1s"));
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

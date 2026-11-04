use crate::ci_policy::ACCESS_LOST_MESSAGE;
use crate::models::CiRun;
/// The branch name a run is stored and shown with. A push arrives as the full ref
/// (`refs/heads/main`) and is stored as `main`, like a manual run. A tag keeps its
/// full ref so it cannot be mistaken for a branch of the same name. Branches whose
/// short names start with `refs/` also keep their full ref to preserve their kind.
pub fn branch_name(reference: &str) -> &str {
    match reference.strip_prefix("refs/heads/") {
        // Keep the kind of branches whose short name looks like a canonical ref.
        Some(name) if !name.starts_with("refs/") => name,
        _ => reference,
    }
}
/// Messages the server writes itself when a run ends. A run's stored message can also come
/// from its runner, which may hold credentials or paths, so only these reach users as written.
const VISIBLE_RUN_ERRORS: &[&str] = &[
    ACCESS_LOST_MESSAGE,
    "A workflow job failed",
    "CI container recovery is unavailable",
    "The CI worker lost contact with the server",
    "CI container cleanup failed; the worker slot is waiting for recovery",
    "CI could not load the repository or secrets",
    "CI could not run",
    "Cancelled",
    "Cancelled before it started",
    "The CI worker shut down during the run",
    "The CI worker stopped before the run finished",
    "The run exceeded its time limit",
];

/// The error a user may see for a run that stored `error`: one of the server's own messages,
/// or a generic line in place of anything the runner wrote.
pub fn visible_run_error(error: Option<&str>) -> Option<String> {
    error.map(|error| {
        if VISIBLE_RUN_ERRORS.contains(&error) {
            error.to_owned()
        } else {
            "The runner recorded an error".to_owned()
        }
    })
}

/// The title and body of the notification a run's starter gets when the run fails.
pub fn failure_notification(run: &CiRun, message: Option<&str>) -> (String, Option<String>) {
    let branch = if matches!(run.event.as_str(), "pull_request" | "pull_request_target") {
        run.branch.as_str()
    } else {
        branch_name(&run.branch)
    };
    let title = match run
        .head_sha
        .as_deref()
        .map(codeyard_webui::components::short_sha)
    {
        Some(short) => format!("CI run failed: {} on {branch} ({short})", run.workflow_path),
        None => format!("CI run failed: {} on {branch}", run.workflow_path),
    };
    (title, visible_run_error(message))
}

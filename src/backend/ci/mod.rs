//! CI/Actions: workflow triggers queue runs; worker nodes run them with `act`.
pub mod admission;
mod config;
pub mod dispatch_inputs;
pub mod logs;
mod process;
pub(crate) mod push;
pub mod queue;
mod runner_labels;
pub mod runs;
pub mod workers;

mod worker;
pub mod workflows;

pub use admission::is_supported_event;
pub use config::{CiConfig, CiExecutionMode, CiPipelineKind};
pub use push::branch_name;
pub use push::RefUpdate;
pub use runs::{CiRunRequest, CiRuns};
pub use workers::CiWorkers;

pub use workflows::{
    commit_message_for_ref, infer_pipeline_from_workflow_path, list_workflows_for_ref,
    should_skip_run_from_message, workflow_name_from_path, WorkflowDefinition,
};

use crate::backend::db::ci::ACCESS_LOST_MESSAGE;
use crate::backend::db::models::CiRun;

/// Messages the server writes itself when a run ends. A run's stored message can also come
/// from its runner, which may hold credentials or paths, so only these reach users as written.
const VISIBLE_RUN_ERRORS: &[&str] = &[
    ACCESS_LOST_MESSAGE,
    "A workflow job failed",
    "CI container recovery is unavailable",
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
    let branch = if run.event == "pull_request" {
        run.branch.as_str()
    } else {
        branch_name(&run.branch)
    };
    let title = match run.head_sha.as_deref().and_then(|sha| sha.get(..7)) {
        Some(short) => format!("CI run failed: {} on {branch} ({short})", run.workflow_path),
        None => format!("CI run failed: {} on {branch}", run.workflow_path),
    };
    (title, visible_run_error(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runners_error_text_is_replaced_and_the_servers_own_messages_pass() {
        assert_eq!(visible_run_error(None), None);
        assert_eq!(
            visible_run_error(Some("Cancelled")).as_deref(),
            Some("Cancelled")
        );
        assert_eq!(
            visible_run_error(Some("CI could not run: clone failed at /srv/git/x.git")).as_deref(),
            Some("The runner recorded an error")
        );
    }
}

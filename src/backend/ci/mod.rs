//! CI/Actions: workflow triggers queue runs; worker nodes run them with `act`.
pub mod admission;

mod config;
pub use codeyard_actions::workflow::dispatch_inputs;
pub mod environment_values;
pub(crate) mod events;
pub mod logs;
pub mod oidc;
mod process;
pub(crate) mod push;
pub mod queue;
mod reusable;
pub mod runner_credentials;
mod runner_labels;
pub mod runs;
pub mod runtime;
pub(crate) mod schedule;
pub mod tokens;
pub mod workers;

mod worker;
pub mod workflows;

pub use admission::is_supported_event;
use codeyard_db_core::ci_messages::branch_name;
pub use config::{CiConfig, CiExecutionMode, CiPipelineKind, RunLimits};
pub use push::RefUpdate;
pub use runs::{CiRunRequest, CiRuns};
pub use workers::CiWorkers;

pub use workflows::{
    commit_message_for_ref, infer_pipeline_from_workflow_path, list_workflows_for_ref,
    should_skip_run_from_message, workflow_name_from_path, WorkflowDefinition,
};

use codeyard_db_core::ci_policy::ACCESS_LOST_MESSAGE;
use codeyard_db_core::models::CiRun;

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

use codeyard_db_core::ci_messages::visible_run_error;

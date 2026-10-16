//! CI/Actions: workflow triggers queue runs; worker nodes run them with `act`.
pub mod admission;
mod config;
pub mod dispatch_inputs;
pub mod logs;
mod process;
mod push;
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

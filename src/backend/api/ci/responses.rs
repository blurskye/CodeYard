use crate::backend::db;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: String,
    pub repo_id: i64,
    pub status: String,
    pub mode: String,
    pub pipeline: String,
    pub event: String,
    pub branch: String,
    pub head_sha: Option<String>,
    pub workflow_path: String,
    pub environment: Option<String>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub created_at: i64,
    pub error_message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkflowInfo {
    pub path: String,
    pub name: String,
    pub supports_push: bool,
    pub supports_pull_request: bool,
    pub supports_workflow_dispatch: bool,
}

pub fn map_job_info(run: db::models::CiRun) -> JobInfo {
    JobInfo {
        id: run.run_id.to_string(),
        repo_id: run.repo_id,
        status: run.status,
        mode: run.mode,
        pipeline: run.pipeline,
        // Pull request bases are literal branch names, not full refs.
        branch: if run.event == "pull_request" {
            run.branch
        } else {
            crate::backend::ci::branch_name(&run.branch).to_owned()
        },
        event: run.event,
        head_sha: run.head_sha,
        workflow_path: run.workflow_path,
        environment: run.environment,
        started_at: run.started_at.map(|v| v.timestamp()),
        completed_at: run.completed_at.map(|v| v.timestamp()),
        created_at: run.created_at.timestamp(),
        error_message: crate::backend::ci::visible_run_error(run.error_message.as_deref()),
    }
}
pub fn map_workflow_info(workflow: crate::backend::ci::WorkflowDefinition) -> WorkflowInfo {
    WorkflowInfo {
        path: workflow.path,
        name: workflow.name,
        supports_push: workflow.triggers.push.is_some(),
        supports_pull_request: workflow.triggers.pull_request.is_some(),
        supports_workflow_dispatch: workflow.triggers.workflow_dispatch,
    }
}

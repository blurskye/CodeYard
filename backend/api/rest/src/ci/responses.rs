use serde::Deserialize;
use serde::Serialize;

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

pub fn map_job_info(run: codeyard_database::models::CiRun) -> JobInfo {
    JobInfo {
        id: run.run_id.to_string(),
        repo_id: run.repo_id,
        status: run.status,
        mode: run.mode,
        pipeline: run.pipeline,
        // Pull request bases are literal branch names, not full refs.
        branch: if matches!(run.event.as_str(), "pull_request" | "pull_request_target") {
            run.branch
        } else {
            codeyard_database::ci_messages::branch_name(&run.branch).to_owned()
        },
        event: run.event,
        head_sha: run.head_sha,
        workflow_path: run.workflow_path,
        environment: run.environment,
        started_at: run.started_at.map(|v| v.timestamp()),
        completed_at: run.completed_at.map(|v| v.timestamp()),
        created_at: run.created_at.timestamp(),
        error_message: codeyard_database::ci_messages::visible_run_error(
            run.error_message.as_deref(),
        ),
    }
}
pub fn map_workflow_info(
    workflow: codeyard_git_storage::workflows::WorkflowDefinition,
) -> WorkflowInfo {
    WorkflowInfo {
        path: workflow.path,
        name: workflow.name,
        supports_push: workflow.triggers.push.is_some(),
        supports_pull_request: workflow.triggers.pull_request.is_some(),
        supports_workflow_dispatch: workflow.triggers.workflow_dispatch,
    }
}

#[cfg(test)]
mod tests {
    use codeyard_api_http::shared::ci::responses::bound_log_projection;
    use codeyard_api_http::shared::ci::responses::workflow_view;
    use codeyard_webui::pages::ci as view;
    use codeyard_webui::pages::repos::actions as view_actions;
    use std::collections::BTreeMap;

    #[test]
    fn workflow_metadata_fits_the_model_budget_without_a_selected_run() {
        use codeyard_git_storage::workflows::WorkflowDefinition;
        use codeyard_runner::workflow::dispatch_inputs::DispatchInput;
        use codeyard_runner::workflow::dispatch_inputs::InputKind;
        use codeyard_runner::workflow::events::WorkflowTriggers;
        let workflow = workflow_view(WorkflowDefinition {
            path: ".github/workflows/ci.yml".into(),
            name: "quote".repeat(100_000),
            triggers: WorkflowTriggers {
                workflow_dispatch: true,
                dispatch_inputs: vec![
                    DispatchInput {
                        name: "target".into(),
                        description: "\"".repeat(200_000),
                        kind: InputKind::Choice,
                        required: false,
                        default: None,
                        options: vec!["\"".repeat(2048); 25]
                    };
                    10
                ],
                ..Default::default()
            },
            jobs: Vec::new(),
        });
        assert!(workflow.inputs[0].description.len() <= 1024);
        let model = view_actions::ActionsPage {
            repository: None,
            repo_chrome: None,
            title: "Actions".into(),
            base_path: "/ci".into(),
            csrf_token: "token".into(),
            query: BTreeMap::new(),
            runs: vec![],
            workflows: vec![workflow; 64],
            selected: None,
            dispatch_branch: "main".into(),
            can_dispatch: true,
            can_manage_runs: true,
            can_cancel: false,
            workflow_warning: None,
            run_limit: 100,
            next_before: None,
            log_limit: 2000,
        };
        let mut envelope = codeyard_webui::app::PageEnvelope {
            title: "Actions".into(),
            location: "/ci".into(),
            chrome: Default::default(),
            page: codeyard_webui::app::Page::Actions(view::Page::Actions(Box::new(model))),
        };
        bound_log_projection(&mut envelope).unwrap();
        assert!(
            serde_json::to_vec(&envelope).unwrap().len()
                < codeyard_webui::bootstrap::MAX_PAGE_MODEL_BYTES
        );
        let codeyard_webui::app::Page::Actions(view::Page::Actions(model)) = envelope.page else {
            panic!("Actions")
        };
        assert!(model.workflow_warning.is_some());
        assert!(model
            .workflows
            .iter()
            .any(|workflow| !workflow.dispatchable && workflow.inputs.is_empty()));
    }
}

use crate::backend::db;
use crate::webui::pages::ci as view;
use crate::webui::pages::repos::actions as view_actions;
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

pub(crate) fn map_run(
    run: db::models::CiRun,
    repository: &db::models::Repository,
) -> view_actions::Run {
    // A pull request run is stored on its base branch, which admission uses. The page shows the head
    // branch of the pull request, which the run kept in its event.
    let head_branch = run
        .pull_request_event
        .as_ref()
        .filter(|_| run.event == "pull_request")
        .and_then(|event| event["head_ref"].as_str());
    // Runs stored before short branch names were used still have the full ref.
    let branch = head_branch
        .unwrap_or_else(|| {
            if run.event == "pull_request" {
                &run.branch
            } else {
                crate::backend::ci::branch_name(&run.branch)
            }
        })
        .to_owned();
    view_actions::Run {
        commit_status_context: run.commit_status_context(),
        id: run.run_id.to_string(),
        repository: repository.name.clone(),
        status: run.status,
        workflow_path: run.workflow_path,
        branch,
        event: run.event,
        pipeline: run.pipeline,
        mode: run.mode,
        head_sha: run.head_sha,
        created_at: run.created_at.timestamp(),
        started_at: run.started_at.map(|value| value.timestamp()),
        completed_at: run.completed_at.map(|value| value.timestamp()),
        error_message: crate::backend::ci::visible_run_error(run.error_message.as_deref()),
        // Admission stores these as an object of strings; the page shows none if it cannot read them.
        inputs: run
            .workflow_inputs
            .and_then(|inputs| serde_json::from_value(inputs).ok())
            .unwrap_or_default(),
    }
}

/// A workflow as the Actions page lists it: its inputs only matter when it can be dispatched.
pub(crate) fn workflow_view(
    workflow: crate::backend::ci::WorkflowDefinition,
) -> view_actions::Workflow {
    view_actions::Workflow {
        path: workflow.path,
        name: workflow.name.chars().take(256).collect(),
        dispatchable: workflow.triggers.workflow_dispatch,
        inputs: workflow
            .triggers
            .dispatch_inputs
            .into_iter()
            .map(workflow_input_view)
            .collect(),
    }
}

fn workflow_input_view(
    input: crate::backend::ci::dispatch_inputs::DispatchInput,
) -> view_actions::WorkflowInput {
    use crate::backend::ci::dispatch_inputs::InputKind;
    view_actions::WorkflowInput {
        name: input.name,
        description: input.description.chars().take(1024).collect(),
        kind: match input.kind {
            InputKind::Text => view_actions::InputKind::Text,
            InputKind::Boolean => view_actions::InputKind::Boolean,
            InputKind::Number => view_actions::InputKind::Number,
            InputKind::Choice => view_actions::InputKind::Choice,
            InputKind::Environment => view_actions::InputKind::Environment,
        },
        required: input.required,
        default: input.default,
        options: input.options,
    }
}

pub(crate) fn bound_log_projection(
    envelope: &mut crate::webui::app::PageEnvelope,
) -> Result<(), serde_json::Error> {
    if let crate::webui::app::Page::Actions(view::Page::Actions(model)) = &mut envelope.page {
        let mut bytes = 0usize;
        for workflow in &mut model.workflows {
            let size = serde_json::to_vec(workflow)?.len();
            if bytes.saturating_add(size) > 2 * 1024 * 1024 {
                workflow.inputs.clear();
                workflow.dispatchable = false;
                model.workflow_warning = Some("Some workflow inputs exceed the page size limit. Use the API to dispatch those workflows.".into());
                bytes += serde_json::to_vec(workflow)?.len();
            } else {
                bytes += size;
            }
        }
    }
    let Some(selected) = selected_run_mut(envelope) else {
        return Ok(());
    };
    let logs = std::mem::take(&mut selected.logs);
    // Count the actual JSON envelope, not raw log bytes: escaping can expand a line.
    let mut bytes = serde_json::to_vec(envelope)?.len();
    let selected = selected_run_mut(envelope).expect("selected run");
    // Keep the newest lines that fit; the browser refuses larger page models.
    for line in logs.into_iter().rev() {
        let size = serde_json::to_vec(&line)?.len() + usize::from(!selected.logs.is_empty());
        if bytes.saturating_add(size) > crate::webui::bootstrap::MAX_PAGE_MODEL_BYTES {
            selected.logs_truncated = true;
            break;
        }
        bytes += size;
        selected.logs.push(line);
    }
    selected.logs.reverse();
    Ok(())
}

fn selected_run_mut(
    envelope: &mut crate::webui::app::PageEnvelope,
) -> Option<&mut view_actions::SelectedRun> {
    match &mut envelope.page {
        crate::webui::app::Page::Actions(view::Page::Actions(model)) => model.selected.as_mut(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn workflow_metadata_fits_the_model_budget_without_a_selected_run() {
        use crate::backend::ci::{
            dispatch_inputs::{DispatchInput, InputKind},
            workflows::{WorkflowDefinition, WorkflowTriggers},
        };
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
        let mut envelope = crate::webui::app::PageEnvelope {
            title: "Actions".into(),
            location: "/ci".into(),
            chrome: Default::default(),
            page: crate::webui::app::Page::Actions(view::Page::Actions(Box::new(model))),
        };
        bound_log_projection(&mut envelope).unwrap();
        assert!(
            serde_json::to_vec(&envelope).unwrap().len()
                < crate::webui::bootstrap::MAX_PAGE_MODEL_BYTES
        );
        let crate::webui::app::Page::Actions(view::Page::Actions(model)) = envelope.page else {
            panic!("Actions")
        };
        assert!(model.workflow_warning.is_some());
        assert!(model
            .workflows
            .iter()
            .any(|workflow| !workflow.dispatchable && workflow.inputs.is_empty()));
    }
}

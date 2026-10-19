//! `push` runs for refs the server moves itself: web editor and file API saves,
//! merges, and new branches and tags. An SSH push queues its runs from the receive
//! hook instead.
use super::{
    infer_pipeline_from_workflow_path, list_workflows_for_ref, workflows::WorkflowMatchBudget,
    CiRunRequest, CiRuns, WorkflowDefinition,
};
use crate::backend::db::models::Repository;
use crate::backend::repositories::store::repo_store;

/// A branch or tag the server has just created or moved.
pub struct RefUpdate {
    /// Full ref name: `refs/heads/main` or `refs/tags/v1`.
    pub reference: String,
    /// Where the ref pointed before; `None` for a new ref.
    pub old: Option<String>,
    pub new: String,
    /// The paths the update changed, when the caller already knows them.
    pub changed_paths: Option<Vec<String>>,
}

impl CiRuns {
    /// Queues the workflows at `update.new` that listen to `push` for this ref, as
    /// `actor_id`. The caller has checked write access and admission checks it again.
    /// The ref has already moved, so a run that cannot be queued is logged and skipped.
    pub async fn queue_push_runs(
        &self,
        repo: &Repository,
        owner: &str,
        actor_id: i64,
        update: RefUpdate,
    ) {
        match self.actor_may_discover_workflows(repo.id, actor_id).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "push runs skipped: CI authority unavailable");
                return;
            }
        }
        let store = repo_store(repo, self.cluster.as_ref());
        let commit = match store.commit(&update.new).await {
            Ok(commit) => commit,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "push runs skipped: commit unreadable");
                return;
            }
        };
        let workflows = match list_workflows_for_ref(&store, &commit.sha).await {
            Ok(workflows) => workflows,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "push runs skipped: workflows unreadable");
                return;
            }
        };
        let workflows: Vec<_> = workflows
            .into_iter()
            .filter(|workflow| workflow.triggers.push.is_some())
            .collect();
        if workflows.is_empty() {
            return;
        }
        // A new branch or tag has nothing to compare with: its path filters do not apply.
        let mut workflows = workflows;
        let changed = match (update.changed_paths, &update.old) {
            (Some(paths), _) => paths,
            (None, Some(old)) if workflows.iter().any(filters_paths) => {
                match store.changed_paths(old, &commit.sha).await {
                    Ok(paths) => paths,
                    Err(error) => {
                        tracing::warn!(repo_id = repo.id, %error, "push runs skipped: changed paths unavailable");
                        return;
                    }
                }
            }
            (None, None) => {
                for filter in workflows
                    .iter_mut()
                    .filter_map(|w| w.triggers.push.as_mut())
                {
                    filter.paths.clear();
                    filter.paths_ignore.clear();
                }
                Vec::new()
            }
            (None, Some(_)) => Vec::new(),
        };
        let mut budget = WorkflowMatchBudget::new();
        let mut matched = Vec::new();
        for workflow in workflows {
            match workflow.matches_event_with_context_and_budget(
                "push",
                &update.reference,
                &changed,
                Some(&commit.message),
                &mut budget,
            ) {
                Ok(true) => matched.push(workflow),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(repo_id = repo.id, %error, "push runs skipped: workflow matching exceeded its work limit");
                    return;
                }
            }
        }
        for workflow in matched {
            let pipeline = infer_pipeline_from_workflow_path(&workflow.path);
            let result = self
                .trigger_run(CiRunRequest {
                    repo_id: repo.id,
                    repo_owner: owner.to_owned(),
                    repo_name: repo.name.clone(),
                    branch: update.reference.clone(),
                    head_sha: Some(commit.sha.clone()),
                    event: "push".to_owned(),
                    pull_request_id: None,
                    pipeline,
                    workflow_paths: vec![workflow.path.clone()],
                    environment: None,
                    inputs: None,
                    triggered_by_id: actor_id,
                    manual: false,
                })
                .await;
            if let Err(error) = result {
                tracing::debug!(repo_id = repo.id, workflow = %workflow.path, %error, "push run not queued");
            }
        }
    }
}

fn filters_paths(workflow: &WorkflowDefinition) -> bool {
    workflow
        .triggers
        .push
        .as_ref()
        .is_some_and(|filter| !filter.paths.is_empty() || !filter.paths_ignore.is_empty())
}

/// The branch name a run is stored and shown with. A push arrives as the full ref
/// (`refs/heads/main`) and is stored as `main`, like a manual run. A tag keeps its
/// full ref so it cannot be mistaken for a branch of the same name.
pub fn branch_name(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

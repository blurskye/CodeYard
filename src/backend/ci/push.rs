//! `push` runs for refs the server moves itself: web editor and file API saves,
//! merges, and new branches and tags. Receive hooks share the same filtering policy.
use super::{
    infer_pipeline_from_workflow_path, list_workflows_for_ref, workflows::WorkflowMatchBudget,
    CiRunRequest, CiRuns, WorkflowDefinition,
};
use crate::backend::db::models::Repository;
use crate::backend::repositories::store::{repo_store, RepoStore};
use anyhow::Result;

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
        if let Err(error) = crate::backend::db::ci::record_ci_accepted_push(
            &self.pool,
            repo.id,
            actor_id,
            &update.reference,
            &commit.sha,
        )
        .await
        {
            tracing::warn!(repo_id = repo.id, %error, "push runs skipped: accepted update could not be recorded");
            return;
        }
        let workflows = match list_workflows_for_ref(&store, &commit.sha).await {
            Ok(workflows) => workflows,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "push runs skipped: workflows unreadable");
                return;
            }
        };
        let mut budget = WorkflowMatchBudget::new();
        let matched = match matching_push_workflows(
            &store,
            &update,
            workflows,
            Some(&commit.message),
            &mut budget,
        )
        .await
        {
            Ok(matched) => matched,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "push workflow matching failed");
                return;
            }
        };
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

/// Ref filters run first. Missing path information skips only workflows that require it.
pub(crate) async fn matching_push_workflows(
    store: &RepoStore,
    update: &RefUpdate,
    workflows: Vec<WorkflowDefinition>,
    message: Option<&str>,
    budget: &mut WorkflowMatchBudget,
) -> Result<Vec<WorkflowDefinition>> {
    let mut candidates = Vec::new();
    for workflow in workflows {
        if workflow.matches_event_with_budget("push", &update.reference, budget)? {
            candidates.push(workflow);
        }
    }
    let needs_paths =
        !update.reference.starts_with("refs/tags/") && candidates.iter().any(filters_paths);
    let changed = if !needs_paths {
        Some(Vec::new())
    } else if update.old.is_some() && update.changed_paths.is_some() {
        update.changed_paths.clone()
    } else {
        let old = update
            .old
            .as_deref()
            .unwrap_or("0000000000000000000000000000000000000000");
        match store.push_changed_paths(old, &update.new).await {
            Ok(paths) => Some(paths),
            Err(error) => {
                tracing::warn!(%error, "push changed paths unavailable; path-filtered workflows skipped");
                None
            }
        }
    };
    let mut matched = Vec::new();
    for workflow in candidates {
        if changed.is_none() && filters_paths(&workflow) {
            continue;
        }
        if workflow.matches_event_with_context_and_budget(
            "push",
            &update.reference,
            changed.as_deref().unwrap_or(&[]),
            message,
            budget,
        )? {
            matched.push(workflow);
        }
    }
    Ok(matched)
}

#[cfg(test)]
#[path = "push_tests.rs"]
mod tests;

fn filters_paths(workflow: &WorkflowDefinition) -> bool {
    workflow
        .triggers
        .push
        .as_ref()
        .is_some_and(|filter| !filter.paths.is_empty() || !filter.paths_ignore.is_empty())
}

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

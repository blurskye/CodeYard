use crate::runs::CiRuns;
use codeyard_db_core::models::Repository;
use codeyard_model::workflow_glob::WorkflowMatchBudget;
use codeyard_repos_core::push_workflows::{matching_push_workflows, push_document, RefUpdate};
use codeyard_repos_core::receive_ci::CiRunRequest;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_repos_core::workflows::{infer_pipeline_from_workflow_path, list_workflows_for_ref};

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
        if update.old.as_deref() == Some(update.new.as_str()) {
            return;
        }
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
        if let Err(error) = codeyard_db_actions::ci::record_ci_accepted_push(
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
        if matched.is_empty() {
            return;
        }
        // A run's github.event retains the published object ID, including annotated tags.
        let event = match push_document(&self.pool, &store, repo, owner, actor_id, &update).await {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "push runs skipped: push document unavailable");
                return;
            }
        };
        for workflow in matched {
            let pipeline = infer_pipeline_from_workflow_path(&workflow.path);
            let result = self
                .trigger_event_run(
                    CiRunRequest {
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
                    },
                    codeyard_db_core::models::AccessLevel::Owner,
                    event.clone(),
                    None,
                )
                .await;
            if let Err(error) = result {
                tracing::debug!(repo_id = repo.id, workflow = %workflow.path, %error, "push run not queued");
            }
        }
    }
}

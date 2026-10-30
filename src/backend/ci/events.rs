//! Repository events use the default branch's workflow and the webhook's unchanged document.
use super::{admission::AdmissionError, workflows::WorkflowMatchBudget, CiRunRequest, CiRuns};
use crate::backend::{
    db::{
        self,
        models::{AccessLevel, Repository},
    },
    repositories::store::repo_store,
    webhooks::payloads,
};
use serde_json::Value;

pub(crate) fn is_repository_event(event: &str) -> bool {
    matches!(event, "repository_dispatch" | "workflow_run")
}

/// GitHub's bounds for a `repository_dispatch` event type and its client payload.
pub(crate) fn valid_dispatch(event_type: &str, client_payload: &Value) -> bool {
    !event_type.is_empty()
        && event_type.chars().count() <= 100
        && !event_type.chars().any(char::is_control)
        && client_payload
            .as_object()
            .is_some_and(|payload| payload.len() <= 10)
        && client_payload.to_string().len() <= 64 * 1024
}

impl CiRuns {
    /// Sends the `repository_dispatch` webhook and queues the default branch's matching
    /// workflows. Callers check write access and `valid_dispatch` first.
    pub(crate) async fn dispatch_repository_event(
        &self,
        repo: &Repository,
        actor: i64,
        sender: i64,
        event_type: String,
        client_payload: Value,
        credential: db::ci::CiCredential,
    ) -> Result<(), AdmissionError> {
        if repo.is_archived {
            return Err(AdmissionError::Denied(
                "Repository was archived so is read-only",
            ));
        }
        let payload = crate::backend::webhooks::emit_repository_dispatch(
            &self.pool, repo, actor, sender, event_type, client_payload, credential.clone(),
        ).await;
        // No payload means its reads failed, so the dispatch did not happen and must not be
        // acknowledged.
        if payload.is_null() {
            return Err(AdmissionError::Unavailable);
        }
        self.queue_repository_event(repo, actor, "repository_dispatch", &payload, Some(credential))
            .await
    }

    pub(crate) async fn emit_workflow_run(&self, run: &db::models::CiRun, action: &str) {
        if action == "requested" && run.workflow_run_attempt.unwrap_or(1) > 1 {
            return;
        }
        if let Err(error) = self.workflow_run_event(run, action).await {
            tracing::warn!(run_id=run.id, %error, "workflow run event was not queued");
        }
    }

    async fn workflow_run_event(
        &self,
        run: &db::models::CiRun,
        action: &str,
    ) -> anyhow::Result<()> {
        let Some(actor) = run.triggered_by_id else {
            return Ok(());
        };
        let Some(repo) =
            db::repositories::records::get_repository_by_id(&self.pool, run.repo_id).await?
        else {
            return Ok(());
        };
        let mut source = run.clone();
        let mut depth = 0;
        let mut may_chain = true;
        while source.event == "workflow_run" {
            depth += 1;
            if depth >= 3 { may_chain = false; break; }
            let Some(id) = source
                .event_payload
                .as_ref()
                .and_then(|payload| payload["workflow_run"]["id"].as_i64())
            else { may_chain = false; break; };
            let Some(previous) = db::ci::get_ci_run_by_id(&self.pool, repo.id, id).await? else {
                may_chain = false; break;
            };
            source = previous;
        }
        let store = repo_store(&repo, self.cluster.as_ref());
        let owner = db::ci::ci_repository_owner(&self.pool, repo.id)
            .await?
            .ok_or(AdmissionError::Unavailable)?;
        let sender = db::users::accounts::get_user_by_id(&self.pool, actor)
            .await?
            .ok_or(AdmissionError::Unavailable)?;
        let metadata = db::action_runs::metadata(&self.pool, repo.id, &[run.id])
            .await?
            .into_iter().next().ok_or(AdmissionError::Unavailable)?;
        let repository = payloads::repository(&repo, &owner);
        let head_repository = if !run.is_fork { repository.clone() } else {
            match metadata.head_repository.as_ref() {
                Some(snapshot) => match db::repositories::records::get_repository_by_id(&self.pool, snapshot.0.id).await.ok().flatten() {
                    Some(head) if head.id == repo.id => repository.clone(),
                    Some(head) if !head.is_private => match db::ci::ci_repository_owner(&self.pool, head.id).await.ok().flatten() {
                        Some(owner) => payloads::repository(&head, &owner),
                        None => Value::Null,
                    },
                    _ => Value::Null,
                },
                None => Value::Null,
            }
        };
        let head_commit = match run.head_sha.as_deref() {
            Some(sha) => store
                .commit(sha)
                .await
                .map_or(Value::Null, |mut commit| {
                    commit.message = payloads::bounded_text(&commit.message, 4096);
                    payloads::simple_commit(&commit)
                }),
            None => Value::Null,
        };
        let workflow_document =
            db::action_workflows::workflow_by_id(&self.pool, repo.id, metadata.workflow_id)
                .await?
                .map_or(Value::Null, |workflow| {
                    payloads::action_workflow(&workflow, &owner, &repo.name)
                });
        let pull = match run.pull_request_id {
            Some(id) => db::pull_requests::records::get_pull_request_by_id(&self.pool, id).await?.filter(|pull| pull.repo_id == repo.id),
            None => None,
        };
        let pull_requests = payloads::action_run_pull_requests(run, pull.as_ref(), &repository, &head_repository, false);
        let mut document = payloads::action_run(
            run,
            &owner,
            repository,
            head_repository,
            head_commit,
            &metadata,
            pull_requests,
        );
        // The webhook always names a start time; a queued attempt reports its creation.
        if document["run_started_at"].is_null() {
            document["run_started_at"] = document["created_at"].clone();
        }
        document["name"] = workflow_document.get("name").filter(|name| name.is_string()).cloned().unwrap_or_else(|| serde_json::json!(run.workflow_path));
        // A rerun keeps the first attempt's actor; whoever reran it is the triggering actor, as
        // in the REST document.
        let first = match run.workflow_run_id.filter(|id| *id != run.id) {
            Some(root) => db::ci::get_ci_run_by_id(&self.pool, repo.id, root)
                .await?
                .and_then(|first| first.triggered_by_id)
                .filter(|id| *id != actor),
            None => None,
        };
        let first = match first {
            Some(id) => db::users::accounts::get_user_by_id(&self.pool, id).await?,
            None => None,
        };
        document["actor"] = payloads::user(first.as_ref().unwrap_or(&sender));
        document["triggering_actor"] = payloads::user(&sender);
        let snapshot = repo.clone();
        let action = action.to_owned();
        let payload = crate::backend::webhooks::emit(
            &self.pool,
            &repo,
            "workflow_run",
            vec![actor],
            move |owner, people| {
                payloads::workflow_run(
                    &action,
                    document,
                    workflow_document,
                    payloads::Scope {
                        repo: &snapshot,
                        owner,
                        sender: &people.get(actor),
                    },
                )
            },
        )
        .await;
        anyhow::ensure!(!payload.is_null(), "workflow run payload could not be emitted");
        if may_chain { self.queue_repository_event(&repo, actor, "workflow_run", &payload, None).await?; }
        Ok(())
    }

    /// Sends `workflow_job` when a native job is queued, starts or finishes.
    pub(crate) async fn emit_workflow_job(&self, run_id: i64, job_id: i64, action: &'static str) {
        if let Err(error) = self.workflow_job_event(run_id, job_id, action).await {
            tracing::warn!(run_id, job_id, %error, "workflow job event was not queued");
        }
    }

    async fn workflow_job_event(
        &self,
        run_id: i64,
        job_id: i64,
        action: &'static str,
    ) -> anyhow::Result<()> {
        let Some(run) = db::ci::get_ci_run(&self.pool, run_id).await? else {
            return Ok(());
        };
        let Some(actor) = run.triggered_by_id else {
            return Ok(());
        };
        let Some(repo) =
            db::repositories::records::get_repository_by_id(&self.pool, run.repo_id).await?
        else {
            return Ok(());
        };
        // The queued event names a job that has not started yet.
        let Some(job) = db::action_jobs::get(&self.pool, repo.id, job_id, true).await? else {
            return Ok(());
        };
        let steps = db::action_jobs::steps(&self.pool, repo.id, job_id).await?;
        let snapshot = repo.clone();
        crate::backend::webhooks::emit(
            &self.pool,
            &repo,
            "workflow_job",
            vec![actor],
            move |owner, people| {
                payloads::workflow_job(
                    action,
                    payloads::action_job(&job, &steps, &run, owner, &snapshot.name),
                    payloads::Scope {
                        repo: &snapshot,
                        owner,
                        sender: &people.get(actor),
                    },
                )
            },
        )
        .await;
        Ok(())
    }

    pub(crate) async fn queue_repository_event(
        &self,
        repo: &Repository,
        actor: i64,
        event: &str,
        payload: &Value,
        credential: Option<db::ci::CiCredential>,
    ) -> Result<(), AdmissionError> {
        if !is_repository_event(event) || !self.actor_may_discover_workflows(repo.id, actor).await?
        {
            return Ok(());
        }
        // With Actions turned off the event is still accepted; it just starts nothing.
        if !db::action_settings::actions_enabled(&self.pool, repo.id).await? {
            return Ok(());
        }
        let store = repo_store(repo, self.cluster.as_ref());
        let reference = format!("refs/heads/{}", repo.default_branch);
        // An empty repository has no default branch commit and so no workflows.
        let commit = match store.commit(&reference).await {
            Ok(commit) => commit,
            Err(error)
                if error.kind == crate::backend::repositories::store::ErrorKind::NotFound =>
            {
                return Ok(())
            }
            Err(error) => return Err(error.into()),
        };
        let definitions = super::list_workflows_for_ref(&store, &commit.sha).await?;
        let mut budget = WorkflowMatchBudget::new();
        let mut matched = Vec::new();
        let branch = if event == "workflow_run" {
            payload["workflow_run"]["head_branch"]
                .as_str()
                .unwrap_or_default()
        } else {
            &repo.default_branch
        };
        for definition in definitions {
            if definition.matches_payload(event, payload)
                && definition
                    .matches_event_with_budget(event, branch, &mut budget)
                    .map_err(|_| {
                        AdmissionError::Invalid("Workflow event matching exceeded its work limit")
                    })?
            {
                matched.push(definition);
            }
        }
        let mut unavailable = false;
        for definition in matched {
            match self
                .trigger_event_run(
                    CiRunRequest {
                        repo_id: repo.id,
                        repo_owner: String::new(),
                        repo_name: repo.name.clone(),
                        branch: reference.clone(),
                        head_sha: Some(commit.sha.clone()),
                        event: event.to_owned(),
                        pull_request_id: None,
                        pipeline: super::infer_pipeline_from_workflow_path(&definition.path),
                        workflow_paths: vec![definition.path],
                        environment: None,
                        inputs: None,
                        triggered_by_id: actor,
                        manual: false,
                    },
                    AccessLevel::Owner,
                    payload.clone(),
                    credential.clone(),
                )
                .await
            {
                Ok(_) | Err(AdmissionError::WorkflowDisabled) => {}
                // One refused listener must not stop the other workflows listening for this event.
                // A storage failure is still reported once the others are queued.
                Err(error) => {
                    unavailable |= matches!(error, AdmissionError::Unavailable);
                    tracing::warn!(repo_id = repo.id, event, %error, "workflow event run was not queued");
                }
            }
        }
        if unavailable {
            return Err(AdmissionError::Unavailable);
        }
        Ok(())
    }
}

//! Admission, enqueue and cancellation of durable CI runs.
use super::admission::AdmissionError;
use super::{
    admission,
    config::{CiConfig, CiPipelineKind},
    push::branch_name,
};
use crate::backend::{
    db::{self, DbPool},
    notifications::NotificationBroadcaster,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Notify;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
pub struct DispatchRequest {
    pub repo_owner: String,
    pub repo_name: String,
    /// Also accepted as `ref`, the name GitHub's workflow dispatch API uses.
    #[serde(alias = "ref")]
    pub branch: String,
    pub event: String,
    pub head_sha: Option<String>,
    pub workflow_path: Option<String>,
    pub pipeline: Option<String>,
    pub environment: Option<String>,
    /// The `workflow_dispatch` inputs: strings, numbers or booleans.
    #[serde(default)]
    pub inputs: Option<BTreeMap<String, serde_json::Value>>,
    /// The repository a web form was rendered for. A form whose owner and name now resolve
    /// to another repository is refused.
    #[serde(skip)]
    pub repo_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CiRunRequest {
    pub repo_id: i64,
    pub repo_owner: String,
    pub repo_name: String,
    pub branch: String,
    pub head_sha: Option<String>,
    pub event: String,
    pub pull_request_id: Option<i64>,
    pub pipeline: CiPipelineKind,
    pub workflow_paths: Vec<String>,
    pub environment: Option<String>,
    /// Submitted `workflow_dispatch` inputs. Admission checks them and replaces them with the
    /// complete set the run gets, defaults included.
    pub inputs: Option<BTreeMap<String, String>>,
    pub triggered_by_id: i64,
    /// Whether a person asked for the run (see `NewCiRun::manual`).
    pub manual: bool,
}

/// A form or API value as the text GitHub shows it in `event.inputs`.
fn input_text(value: serde_json::Value) -> Result<String, AdmissionError> {
    match value {
        serde_json::Value::String(text) => Ok(text),
        serde_json::Value::Bool(flag) => Ok(flag.to_string()),
        serde_json::Value::Number(number) => Ok(number.to_string()),
        _ => Err(AdmissionError::InvalidRequest(
            "workflow input values must be strings, numbers or booleans",
        )),
    }
}

#[derive(Clone)]
pub struct CiRuns {
    pub(super) pool: DbPool,
    pub cluster: Option<crate::backend::cluster::NodeContext>,
    pub(crate) config: Arc<CiConfig>,
    pub(crate) queued: Arc<Notify>,
    notifications: Arc<NotificationBroadcaster>,
}

impl CiRuns {
    /// The broadcaster that carries notifications to live sockets. Pull request paths that send
    /// notifications outside a request reach it through the CI handle they already hold.
    pub fn notifications(&self) -> &NotificationBroadcaster {
        &self.notifications
    }

    pub fn new(
        pool: DbPool,
        config: Arc<CiConfig>,
        queued: Arc<Notify>,
        cluster: Option<crate::backend::cluster::NodeContext>,
        notifications: Arc<NotificationBroadcaster>,
    ) -> Self {
        Self {
            pool,
            config,
            queued,
            cluster,
            notifications,
        }
    }

    /// Manual JSON and native forms share validation and immutable admission.
    pub async fn dispatch_run(
        &self,
        actor_id: i64,
        request: DispatchRequest,
    ) -> Result<db::models::CiRun, AdmissionError> {
        use super::admission::{is_safe_owner, is_safe_ref, is_safe_workflow_path};
        use crate::backend::git::is_full_commit_id;
        if !is_safe_owner(&request.repo_owner)
            || crate::backend::repositories::lifecycle::validate_name(&request.repo_name).is_err()
        {
            return Err(AdmissionError::InvalidRequest(
                "repo_owner and repo_name must use safe repository characters",
            ));
        }
        let reference = if request.event == "pull_request" {
            format!("refs/heads/{}", request.branch)
        } else {
            request.branch.clone()
        };
        if !is_safe_ref(&reference) {
            return Err(AdmissionError::InvalidRequest(
                "branch contains unsupported characters",
            ));
        }
        if !matches!(
            request.event.as_str(),
            "push" | "pull_request" | "workflow_dispatch"
        ) {
            return Err(AdmissionError::InvalidRequest(
                "supported events are push, pull_request and workflow_dispatch",
            ));
        }
        if request
            .head_sha
            .as_deref()
            .is_some_and(|sha| !is_full_commit_id(sha))
        {
            return Err(AdmissionError::InvalidRequest(
                "head_sha must be a full 40-character hexadecimal commit id",
            ));
        }
        let repository = db::repositories::records::get_repository(
            &self.pool,
            &request.repo_owner,
            &request.repo_name,
        )
        .await?
        .ok_or(AdmissionError::Missing("Repository not found"))?;
        if request.repo_id.is_some_and(|id| id != repository.id) {
            return Err(AdmissionError::Conflict(
                "This form belongs to another repository; reload the page",
            ));
        }
        let user = db::users::accounts::get_user_by_id(&self.pool, actor_id)
            .await?
            .ok_or(AdmissionError::Unavailable)?;
        if !crate::backend::repositories::access::can_read(&self.pool, Some(&user), &repository)
            .await?
        {
            return Err(AdmissionError::Missing("Repository not found"));
        }
        let selected = request.pipeline.as_deref().unwrap_or("ci");
        if !matches!(selected, "ci" | "cd" | "full") {
            return Err(AdmissionError::InvalidRequest(
                "pipeline must be ci, cd or full",
            ));
        }
        let mut pipeline = CiPipelineKind::from_value(selected);
        let workflow_paths = if let Some(path) = request.workflow_path {
            if !is_safe_workflow_path(&path) {
                return Err(AdmissionError::InvalidRequest(
                    "workflow_path must be under .github/workflows and end with .yml or .yaml",
                ));
            }
            pipeline = super::infer_pipeline_from_workflow_path(&path);
            vec![path]
        } else {
            self.config.workflow_paths(pipeline)
        };
        let environment = request
            .environment
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let inputs = request
            .inputs
            .map(|inputs| {
                inputs
                    .into_iter()
                    .map(|(name, value)| Ok::<_, AdmissionError>((name, input_text(value)?)))
                    .collect::<Result<BTreeMap<_, _>, AdmissionError>>()
            })
            .transpose()?;
        self.trigger_run(CiRunRequest {
            repo_id: repository.id,
            repo_owner: request.repo_owner,
            repo_name: request.repo_name,
            branch: request.branch,
            head_sha: request.head_sha,
            event: request.event,
            pull_request_id: None,
            pipeline,
            workflow_paths,
            environment,
            inputs,
            triggered_by_id: actor_id,
            manual: true,
        })
        .await
    }

    /// Starts another attempt from a stored run, repeating current execution and PR checks.
    pub async fn rerun_run(
        &self,
        run_id: Uuid,
        actor_id: i64,
    ) -> Result<db::models::CiRun, AdmissionError> {
        self.rerun_run_options(
            run_id,
            actor_id,
            false,
            db::ci::CiReplaySelection::All,
            None,
            true,
        )
        .await
    }

    /// Starts a new attempt of one logical workflow run from its immutable native snapshot.
    pub async fn rerun_workflow_run(
        &self,
        repo: i64,
        id: i64,
        actor: i64,
        debug: bool,
        credential: Option<i64>,
    ) -> Result<db::models::CiRun, AdmissionError> {
        self.rerun_selected_workflow(
            repo,
            id,
            actor,
            debug,
            credential,
            db::ci::CiReplaySelection::All,
        )
        .await
    }
    pub async fn rerun_failed_workflow_run(
        &self,
        repo: i64,
        id: i64,
        actor: i64,
        debug: bool,
        credential: Option<i64>,
    ) -> Result<db::models::CiRun, AdmissionError> {
        self.rerun_selected_workflow(
            repo,
            id,
            actor,
            debug,
            credential,
            db::ci::CiReplaySelection::Failed,
        )
        .await
    }
    pub async fn rerun_workflow_job(
        &self,
        repo: i64,
        id: i64,
        actor: i64,
        debug: bool,
        credential: Option<i64>,
    ) -> Result<db::models::CiRun, AdmissionError> {
        let job = db::action_jobs::get(&self.pool, repo, id, false)
            .await?
            .ok_or(AdmissionError::Missing("Job not found"))?;
        let source = db::ci::get_ci_run_by_id(&self.pool, repo, job.run_id)
            .await?
            .ok_or(AdmissionError::Missing("Run not found"))?;
        self.rerun_selected_workflow(
            repo,
            source.workflow_run_id.unwrap_or(source.id),
            actor,
            debug,
            credential,
            db::ci::CiReplaySelection::Job(id),
        )
        .await
    }
    async fn rerun_selected_workflow(
        &self,
        repo: i64,
        id: i64,
        actor: i64,
        debug: bool,
        credential: Option<i64>,
        selection: db::ci::CiReplaySelection,
    ) -> Result<db::models::CiRun, AdmissionError> {
        let source = db::action_runs::get(&self.pool, repo, id, None)
            .await?
            .ok_or(AdmissionError::Missing("Run not found"))?;
        if !source.workflow_run_number.is_some_and(|number| number > 0) {
            return Err(AdmissionError::Conflict(
                "Source workflow metadata is unavailable",
            ));
        }
        self.rerun_run_options(source.run_id, actor, debug, selection, credential, false)
            .await
    }

    async fn rerun_run_options(
        &self,
        run_id: Uuid,
        actor_id: i64,
        debug: bool,
        selection: db::ci::CiReplaySelection,
        credential: Option<i64>,
        allow_active: bool,
    ) -> Result<db::models::CiRun, AdmissionError> {
        let run = db::ci::get_ci_run_by_run_id(&self.pool, run_id)
            .await?
            .ok_or(AdmissionError::Missing("Run not found"))?;
        let attempt = run
            .workflow_run_number
            .filter(|number| *number > 0)
            .map(|_| db::ci::NewCiAttempt {
                source: run.id,
                source_uuid: run.run_id,
                credential,
                allow_active,
                selection,
                definition: None,
                debug,
            });
        let head_sha = run.head_sha.ok_or(AdmissionError::Conflict(
            "This run has no commit to run again",
        ))?;
        let inputs = run
            .workflow_inputs
            .map(serde_json::from_value::<BTreeMap<String, String>>)
            .transpose()
            .map_err(|_| AdmissionError::Invalid("Stored workflow inputs are invalid"))?;
        self.enqueue_run(
            CiRunRequest {
                repo_id: run.repo_id,
                repo_owner: String::new(),
                repo_name: String::new(),
                branch: run.branch,
                head_sha: Some(head_sha),
                event: run.event,
                pull_request_id: run.pull_request_id,
                pipeline: CiPipelineKind::from_value(&run.pipeline),
                workflow_paths: run.workflow_path.split(',').map(str::to_owned).collect(),
                environment: run.environment,
                inputs,
                triggered_by_id: actor_id,
                manual: true,
            },
            true,
            db::models::AccessLevel::Owner,
            attempt,
            run.event_payload,
            Some(run.workflow_blobs),
        )
        .await
    }

    /// Cheap current-authority check before repository-controlled workflow discovery.
    /// Durable admission repeats this check in the enqueue transaction.
    pub(crate) async fn actor_may_discover_workflows(
        &self,
        repo_id: i64,
        actor_id: i64,
    ) -> Result<bool> {
        if !db::action_settings::actions_enabled(&self.pool, repo_id).await? { return Ok(false); }
        let Some(actor) = db::users::accounts::get_user_by_id(&self.pool, actor_id).await? else {
            return Ok(false);
        };
        let Some(repository) =
            db::repositories::records::get_repository_by_id(&self.pool, repo_id).await?
        else {
            return Ok(false);
        };
        let access =
            db::repositories::access::get_user_repo_access(&self.pool, actor.id, repository.id)
                .await?;
        Ok(admission::check_actor(&self.config, &actor, &repository, access).is_ok())
    }

    pub fn config(&self) -> &CiConfig {
        &self.config
    }

    /// Checks the request and queues a run. A worker node picks it up.
    pub async fn trigger_run(
        &self,
        request: CiRunRequest,
    ) -> Result<db::models::CiRun, AdmissionError> {
        self.trigger_run_with_access(request, db::models::AccessLevel::Owner)
            .await
    }

    /// A receive credential may grant less than its account (notably a deploy key).
    pub(crate) async fn trigger_run_with_access(
        &self,
        request: CiRunRequest,
        access: db::models::AccessLevel,
    ) -> Result<db::models::CiRun, AdmissionError> {
        self.enqueue_run(request, false, access, None, None, None)
            .await
    }

    pub(crate) async fn trigger_event_run(
        &self,
        request: CiRunRequest,
        access: db::models::AccessLevel,
        payload: serde_json::Value,
    ) -> Result<db::models::CiRun, AdmissionError> {
        self.enqueue_run(request, false, access, None, Some(payload), None)
            .await
    }

    async fn enqueue_run(
        &self,
        request: CiRunRequest,
        rerun: bool,
        access: db::models::AccessLevel,
        mut attempt: Option<db::ci::NewCiAttempt>,
        event_payload: Option<serde_json::Value>,
        source_blobs: Option<serde_json::Value>,
    ) -> Result<db::models::CiRun, AdmissionError> {
        let revision = admission::resolve(
            &self.pool,
            &self.config,
            self.cluster.as_ref(),
            request,
            rerun,
            event_payload.as_ref(),
            source_blobs.as_ref(),
        )
        .await?;
        let request = revision.request;
        let event_payload = if event_payload.is_none() && request.event == "workflow_dispatch" {
            let repository = db::repositories::records::get_repository_by_id(&self.pool, request.repo_id).await?.ok_or(AdmissionError::Missing("CI repository missing"))?;
            let sender = db::users::accounts::get_user_by_id(&self.pool, request.triggered_by_id).await?.ok_or(AdmissionError::Denied("CI actor missing"))?;
            Some(serde_json::json!({
                "ref": request.branch,
                "inputs": request.inputs,
                "repository": crate::backend::webhooks::payloads::repository(&repository, &request.repo_owner),
                "sender": crate::backend::webhooks::payloads::user(&sender),
            }))
        } else { event_payload };
        if let Some(attempt) = attempt
            .as_mut()
            .filter(|attempt| attempt.selection != db::ci::CiReplaySelection::All)
        {
            if request.workflow_paths.len() != 1 {
                return Err(AdmissionError::Conflict(
                    "Partial re-runs require one workflow",
                ));
            }
            // A workflow with calls is stored as its rewritten snapshot; replay reads that text.
            let path = &request.workflow_paths[0];
            attempt.definition = match revision.blobs.get(path).and_then(super::reusable::content) {
                Some(content) => Some(content.to_owned()),
                None => revision.definitions.get(path).cloned(),
            };
        }
        // YAML environments and configured workflow names can promote CI to deployment.
        if request.pipeline.includes_cd()
            && !access.can_admin()
            && !admission::fork_approval_required(&self.pool, &request).await?
        {
            return Err(AdmissionError::Denied("CI deployment permission denied"));
        }
        let head_sha = request
            .head_sha
            .as_deref()
            .ok_or(AdmissionError::Invalid("CI needs an exact commit"))?;
        let workflow_inputs = request
            .inputs
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| AdmissionError::Unavailable)?;
        let trusted: Vec<i64> = self.config.trusted_actor_ids.iter().copied().collect();
        let input = db::ci::NewCiRun {
            repo_id: request.repo_id,
            triggered_by_id: request.triggered_by_id,
            pipeline: request.pipeline.as_str(),
            event: &request.event,
            pull_request_id: request.pull_request_id,
            branch: if matches!(
                request.event.as_str(),
                "pull_request" | "pull_request_target"
            ) {
                &request.branch
            } else {
                branch_name(&request.branch)
            },
            head_sha,
            workflow_path: &request.workflow_paths.join(","),
            workflow_blobs: &revision.blobs,
            workflow_inputs: workflow_inputs.as_ref(),
            environment: request.environment.as_deref(),
            manual: request.manual,
        };
        let run = match attempt {
            Some(attempt) => {
                db::ci::enqueue_ci_run_attempt(
                    &self.pool,
                    input,
                    &trusted,
                    attempt,
                    &revision.scheduling,
                    event_payload.as_ref(),
                )
                .await?
            }
            None => {
                db::ci::enqueue_ci_run_for_runners(
                    &self.pool,
                    input,
                    &trusted,
                    &revision.scheduling,
                    event_payload.as_ref(),
                )
                .await?
            }
        };
        if let Err(error) =
            crate::backend::webhooks::deployment_protection::requested(&self.pool, &run).await
        {
            tracing::warn!(run_id=run.id,%error,"Could not queue deployment protection requests");
        }
        self.queued.notify_one();
        Box::pin(self.emit_workflow_run(&run, "requested")).await;
        Ok(run)
    }

    /// Cancels a queued run, or asks the worker running it to stop.
    pub async fn cancel_run(&self, run_id: Uuid, actor_id: i64) -> Result<bool, AdmissionError> {
        self.cancel_run_with_token(run_id, actor_id, None).await
    }

    pub async fn cancel_run_with_token(
        &self,
        run_id: Uuid,
        actor_id: i64,
        token_id: Option<i64>,
    ) -> Result<bool, AdmissionError> {
        let (changed, notifications) = match token_id {
            Some(token) => {
                db::ci::request_ci_cancellation_with_token(&self.pool, run_id, actor_id, token)
                    .await?
            }
            None => db::ci::request_ci_cancellation(&self.pool, run_id, actor_id).await?,
        };
        self.notifications.send_created(&notifications).await;
        if changed {
            if let Some(run) = db::ci::get_ci_run_by_run_id(&self.pool, run_id).await? {
                if run.completed_at.is_some() {
                    self.emit_workflow_run(&run, "completed").await;
                }
            }
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn workflow_discovery_preflight_requires_current_execution_authority() {
        let native = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let pool = DbPool::from(native.clone());
        db::run_migrations(&pool).await.unwrap();
        let owner = db::users::accounts::create_user(&pool, "preflight_owner", None)
            .await
            .unwrap();
        let actor = db::users::accounts::create_user(&pool, "preflight_actor", None)
            .await
            .unwrap();
        let repository = db::repositories::records::create_repository(
            &pool,
            owner.id,
            "preflight",
            None,
            true,
            "/unused/preflight.git",
        )
        .await
        .unwrap();
        let mut config = CiConfig::default();
        config.trusted_actor_ids.insert(actor.id);
        let service = CiRuns::new(
            pool.clone(),
            Arc::new(config),
            Arc::new(Notify::new()),
            None,
            Arc::new(NotificationBroadcaster::new(&pool)),
        );

        assert!(!service
            .actor_may_discover_workflows(repository.id, actor.id)
            .await
            .unwrap());
        db::repositories::access::grant_repo_access(
            &pool,
            actor.id,
            repository.id,
            db::models::AccessLevel::Write,
        )
        .await
        .unwrap();
        assert!(service
            .actor_may_discover_workflows(repository.id, actor.id)
            .await
            .unwrap());

        sqlx::query("UPDATE users SET is_banned=TRUE WHERE id=?1")
            .bind(actor.id)
            .execute(&native)
            .await
            .unwrap();
        assert!(!service
            .actor_may_discover_workflows(repository.id, actor.id)
            .await
            .unwrap());
        pool.close().await;
    }
}

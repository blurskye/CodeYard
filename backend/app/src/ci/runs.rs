//! Admission, enqueue and cancellation of durable CI runs.
use codeyard_ci::config::CiConfig;
use codeyard_ci::config::CiPipelineKind;
use anyhow::Result;
use codeyard_db_core::ci_admission::AdmissionError;
use codeyard_db_core::ci_messages::branch_name;
use codeyard_db_core::DbPool;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use serde::Deserialize;
use serde::Serialize;
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
    pub pool: DbPool,
    pub cluster: Option<crate::cluster::context::NodeContext>,
    pub config: Arc<CiConfig>,
    pub queued: Arc<Notify>,
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
        cluster: Option<crate::cluster::context::NodeContext>,
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
        credential: Option<codeyard_db_core::queries::ci::CiCredential>,
    ) -> Result<codeyard_db_core::models::CiRun, AdmissionError> {
        use crate::ci::admission::is_safe_owner;
        use crate::ci::admission::is_safe_ref;
        use crate::ci::admission::is_safe_workflow_path;
        use codeyard_git::is_full_commit_id;
        if !is_safe_owner(&request.repo_owner)
            || crate::repositories::lifecycle::validate_name(&request.repo_name).is_err()
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
        let repository = codeyard_db_repos::repositories::records::get_repository(
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
        let user = codeyard_db_identity::users::accounts::get_user_by_id(&self.pool, actor_id)
            .await?
            .ok_or(AdmissionError::Unavailable)?;
        if !codeyard_identity::repository_access::can_read(&self.pool, Some(&user), &repository)
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
            pipeline = crate::ci::workflows::infer_pipeline_from_workflow_path(&path);
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
        self.enqueue_run(
            CiRunRequest {
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
            },
            false,
            codeyard_db_core::models::AccessLevel::Owner,
            None,
            None,
            None,
            credential,
        )
        .await
    }

    /// Starts another attempt from a stored run, repeating current execution and PR checks.
    pub async fn rerun_run(
        &self,
        run_id: Uuid,
        actor_id: i64,
        credential: Option<codeyard_db_core::queries::ci::CiCredential>,
    ) -> Result<codeyard_db_core::models::CiRun, AdmissionError> {
        self.rerun_run_options(
            run_id,
            actor_id,
            false,
            codeyard_db_actions::ci::CiReplaySelection::All,
            credential,
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
        credential: Option<codeyard_db_core::queries::ci::CiCredential>,
    ) -> Result<codeyard_db_core::models::CiRun, AdmissionError> {
        self.rerun_selected_workflow(
            repo,
            id,
            actor,
            debug,
            credential,
            codeyard_db_actions::ci::CiReplaySelection::All,
        )
        .await
    }
    pub async fn rerun_failed_workflow_run(
        &self,
        repo: i64,
        id: i64,
        actor: i64,
        debug: bool,
        credential: Option<codeyard_db_core::queries::ci::CiCredential>,
    ) -> Result<codeyard_db_core::models::CiRun, AdmissionError> {
        self.rerun_selected_workflow(
            repo,
            id,
            actor,
            debug,
            credential,
            codeyard_db_actions::ci::CiReplaySelection::Failed,
        )
        .await
    }
    pub async fn rerun_workflow_job(
        &self,
        repo: i64,
        id: i64,
        actor: i64,
        debug: bool,
        credential: Option<codeyard_db_core::queries::ci::CiCredential>,
    ) -> Result<codeyard_db_core::models::CiRun, AdmissionError> {
        let job = codeyard_db_actions::action_jobs::get(&self.pool, repo, id, false)
            .await?
            .ok_or(AdmissionError::Missing("Job not found"))?;
        let source = codeyard_db_actions::ci::get_ci_run_by_id(&self.pool, repo, job.run_id)
            .await?
            .ok_or(AdmissionError::Missing("Run not found"))?;
        self.rerun_selected_workflow(
            repo,
            source.workflow_run_id.unwrap_or(source.id),
            actor,
            debug,
            credential,
            codeyard_db_actions::ci::CiReplaySelection::Job(id),
        )
        .await
    }
    async fn rerun_selected_workflow(
        &self,
        repo: i64,
        id: i64,
        actor: i64,
        debug: bool,
        credential: Option<codeyard_db_core::queries::ci::CiCredential>,
        selection: codeyard_db_actions::ci::CiReplaySelection,
    ) -> Result<codeyard_db_core::models::CiRun, AdmissionError> {
        let source = codeyard_db_actions::action_runs::get(&self.pool, repo, id, None)
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
        selection: codeyard_db_actions::ci::CiReplaySelection,
        credential: Option<codeyard_db_core::queries::ci::CiCredential>,
        allow_active: bool,
    ) -> Result<codeyard_db_core::models::CiRun, AdmissionError> {
        let run = codeyard_db_actions::ci::get_ci_run_by_run_id(&self.pool, run_id)
            .await?
            .ok_or(AdmissionError::Missing("Run not found"))?;
        let attempt = run
            .workflow_run_number
            .filter(|number| *number > 0)
            .map(|_| codeyard_db_actions::ci::NewCiAttempt {
                source: run.id,
                source_uuid: run.run_id,
                credential: credential.clone(),
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
            codeyard_db_core::models::AccessLevel::Owner,
            attempt,
            run.event_payload,
            Some(run.workflow_blobs),
            credential,
        )
        .await
    }

    /// Cheap current-authority check before repository-controlled workflow discovery.
    /// Durable admission repeats this check in the enqueue transaction.
    pub async fn actor_may_discover_workflows(&self, repo_id: i64, actor_id: i64) -> Result<bool> {
        if !codeyard_db_actions::action_settings::actions_enabled(&self.pool, repo_id).await? {
            return Ok(false);
        }
        let Some(actor) =
            codeyard_db_identity::users::accounts::get_user_by_id(&self.pool, actor_id).await?
        else {
            return Ok(false);
        };
        let Some(repository) =
            codeyard_db_repos::repositories::records::get_repository_by_id(&self.pool, repo_id)
                .await?
        else {
            return Ok(false);
        };
        let access = codeyard_db_repos::repositories::access::get_user_repo_access(
            &self.pool,
            actor.id,
            repository.id,
        )
        .await?;
        Ok(crate::ci::admission::check_actor(&self.config, &actor, &repository, access).is_ok())
    }

    pub fn config(&self) -> &CiConfig {
        &self.config
    }

    /// Checks the request and queues a run. A worker node picks it up.
    pub async fn trigger_run(
        &self,
        request: CiRunRequest,
    ) -> Result<codeyard_db_core::models::CiRun, AdmissionError> {
        self.trigger_run_with_access(request, codeyard_db_core::models::AccessLevel::Owner)
            .await
    }

    /// A receive credential may grant less than its account (notably a deploy key).
    pub(crate) async fn trigger_run_with_access(
        &self,
        request: CiRunRequest,
        access: codeyard_db_core::models::AccessLevel,
    ) -> Result<codeyard_db_core::models::CiRun, AdmissionError> {
        self.enqueue_run(request, false, access, None, None, None, None)
            .await
    }

    pub(crate) async fn trigger_event_run(
        &self,
        request: CiRunRequest,
        access: codeyard_db_core::models::AccessLevel,
        payload: serde_json::Value,
        credential: Option<codeyard_db_core::queries::ci::CiCredential>,
    ) -> Result<codeyard_db_core::models::CiRun, AdmissionError> {
        self.enqueue_run(
            request,
            false,
            access,
            None,
            Some(payload),
            None,
            credential,
        )
        .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Admission takes the request, its access, attempt, payloads and credential separately."
    )]
    async fn enqueue_run(
        &self,
        request: CiRunRequest,
        rerun: bool,
        access: codeyard_db_core::models::AccessLevel,
        mut attempt: Option<codeyard_db_actions::ci::NewCiAttempt>,
        event_payload: Option<serde_json::Value>,
        source_blobs: Option<serde_json::Value>,
        credential: Option<codeyard_db_core::queries::ci::CiCredential>,
    ) -> Result<codeyard_db_core::models::CiRun, AdmissionError> {
        let revision = crate::ci::admission::resolve(
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
            let repository = codeyard_db_repos::repositories::records::get_repository_by_id(
                &self.pool,
                request.repo_id,
            )
            .await?
            .ok_or(AdmissionError::Missing("CI repository missing"))?;
            let sender = codeyard_db_identity::users::accounts::get_user_by_id(
                &self.pool,
                request.triggered_by_id,
            )
            .await?
            .ok_or(AdmissionError::Denied("CI actor missing"))?;
            Some(serde_json::json!({
                "ref": request.branch,
                "inputs": request.inputs,
                "repository": crate::webhooks::payloads::repository(&repository, &request.repo_owner),
                "sender": crate::webhooks::payloads::user(&sender),
            }))
        } else {
            event_payload
        };
        if let Some(attempt) = attempt
            .as_mut()
            .filter(|attempt| attempt.selection != codeyard_db_actions::ci::CiReplaySelection::All)
        {
            if request.workflow_paths.len() != 1 {
                return Err(AdmissionError::Conflict(
                    "Partial re-runs require one workflow",
                ));
            }
            // A workflow with calls is stored as its rewritten snapshot; replay reads that text.
            let path = &request.workflow_paths[0];
            attempt.definition = match revision
                .blobs
                .get(path)
                .and_then(crate::ci::reusable::content)
            {
                Some(content) => Some(content.to_owned()),
                None => revision.definitions.get(path).cloned(),
            };
        }
        // YAML environments and configured workflow names can promote CI to deployment.
        if request.pipeline.includes_cd()
            && !access.can_admin()
            && !crate::ci::admission::fork_approval_required(&self.pool, &request).await?
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
        let input = codeyard_db_actions::ci::NewCiRun {
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
        let (run, inserted) = match attempt {
            Some(attempt) => {
                codeyard_db_actions::ci::enqueue_ci_run_attempt(
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
                codeyard_db_actions::ci::enqueue_ci_run_for_runners(
                    &self.pool,
                    input,
                    &trusted,
                    &revision.scheduling,
                    event_payload.as_ref(),
                    credential.as_ref(),
                )
                .await?
            }
        };
        self.queued.notify_one();
        if !inserted {
            return Ok(run);
        }
        if let Err(error) =
            crate::webhooks::deployment_protection::requested(&self.pool, &run).await
        {
            tracing::warn!(run_id=run.id,%error,"Could not queue deployment protection requests");
        }
        Box::pin(self.emit_workflow_run(&run, "requested")).await;
        Ok(run)
    }

    /// Cancels a queued run, or asks the worker running it to stop.
    pub async fn cancel_run(
        &self,
        run_id: Uuid,
        actor_id: i64,
        credential: Option<codeyard_db_core::queries::ci::CiCredential>,
    ) -> Result<bool, AdmissionError> {
        let (changed, notifications, completed) = match credential {
            Some(proof) => {
                codeyard_db_actions::ci::request_ci_cancellation_with_credential(
                    &self.pool, run_id, actor_id, &proof,
                )
                .await?
            }
            None => {
                codeyard_db_actions::ci::request_ci_cancellation(&self.pool, run_id, actor_id)
                    .await?
            }
        };
        self.notifications.send_created(&notifications).await;
        if let Some(run) = completed {
            self.emit_workflow_run(&run, "completed").await;
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
        codeyard_db_core::run_migrations(&pool).await.unwrap();
        let owner =
            codeyard_db_identity::users::accounts::create_user(&pool, "preflight_owner", None)
                .await
                .unwrap();
        let actor =
            codeyard_db_identity::users::accounts::create_user(&pool, "preflight_actor", None)
                .await
                .unwrap();
        let repository = codeyard_db_repos::repositories::records::create_repository(
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
        codeyard_db_repos::repositories::access::grant_repo_access(
            &pool,
            actor.id,
            repository.id,
            codeyard_db_core::models::AccessLevel::Write,
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

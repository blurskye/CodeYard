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
        use super::admission::{
            is_safe_owner, is_safe_ref, is_safe_workflow_path, is_supported_event,
            is_valid_sha1_hex,
        };
        if !is_safe_owner(&request.repo_owner)
            || crate::backend::repositories::lifecycle::validate_name(&request.repo_name).is_err()
        {
            return Err(AdmissionError::InvalidRequest(
                "repo_owner and repo_name must use safe repository characters",
            ));
        }
        if !is_safe_ref(&request.branch) {
            return Err(AdmissionError::InvalidRequest(
                "branch contains unsupported characters",
            ));
        }
        if !is_supported_event(&request.event) {
            return Err(AdmissionError::InvalidRequest(
                "supported events are push, pull_request and workflow_dispatch",
            ));
        }
        if request
            .head_sha
            .as_deref()
            .is_some_and(|sha| !is_valid_sha1_hex(sha))
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

    /// Cheap current-authority check before repository-controlled workflow discovery.
    /// Durable admission repeats this check in the enqueue transaction.
    pub(crate) async fn actor_may_discover_workflows(
        &self,
        repo_id: i64,
        actor_id: i64,
    ) -> Result<bool> {
        let Some(actor) = db::users::accounts::get_user_by_id(&self.pool, actor_id).await? else {
            return Ok(false);
        };
        if actor.is_banned
            || actor.must_change_password
            || (!actor.is_admin && !self.config.trusted_actor_ids.contains(&actor.id))
        {
            return Ok(false);
        }
        let Some(repository) =
            db::repositories::records::get_repository_by_id(&self.pool, repo_id).await?
        else {
            return Ok(false);
        };
        if repository.is_archived {
            return Ok(false);
        }
        let access =
            db::repositories::access::get_user_repo_access(&self.pool, actor.id, repository.id)
                .await?;
        let personal_admin = actor.is_admin
            && repository.owner_kind == "user"
            && repository.owner_user_id.is_some()
            && repository.owner_org_id.is_none();
        Ok(personal_admin || access.can_write())
    }

    pub fn config(&self) -> &CiConfig {
        &self.config
    }

    /// Checks the request and queues a run. A worker node picks it up.
    pub async fn trigger_run(
        &self,
        request: CiRunRequest,
    ) -> Result<db::models::CiRun, AdmissionError> {
        let revision =
            admission::resolve(&self.pool, &self.config, self.cluster.as_ref(), request).await?;
        let request = revision.request;
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
        let run = db::ci::enqueue_ci_run(
            &self.pool,
            db::ci::NewCiRun {
                repo_id: request.repo_id,
                triggered_by_id: request.triggered_by_id,
                pipeline: request.pipeline.as_str(),
                event: &request.event,
                pull_request_id: request.pull_request_id,
                branch: branch_name(&request.branch),
                head_sha,
                workflow_path: &request.workflow_paths.join(","),
                workflow_blobs: &revision.blobs,
                workflow_inputs: workflow_inputs.as_ref(),
                environment: request.environment.as_deref(),
                manual: request.manual,
            },
            &trusted,
        )
        .await?;
        self.queued.notify_one();
        Ok(run)
    }

    /// Cancels a queued run, or asks the worker running it to stop.
    pub async fn cancel_run(&self, run_id: Uuid, actor_id: i64) -> Result<bool, AdmissionError> {
        let (changed, notifications) =
            db::ci::request_ci_cancellation(&self.pool, run_id, actor_id).await?;
        self.notifications.send_created(&notifications).await;
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

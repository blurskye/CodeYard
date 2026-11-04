//! One CI queue, reached through the database or a paired core node.
use crate::backend::ci::config::RunLimits;
use crate::backend::ci::CiConfig;
use crate::backend::ci::CiExecutionMode;

use crate::backend::cluster::NodeContext;
use codeyard_support::secrets::SecretEncryptor;

use crate::backend::repositories::store::repo_store;
use crate::backend::repositories::store::RepoStore;
use anyhow::{ensure, Context, Result};
use codeyard_db_core::models::CiRun;
use codeyard_db_core::DbPool;
use codeyard_model::ci::LogLine;
use codeyard_notifications::NotificationBroadcaster;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

/// The core answered and refused the request, so sending it again cannot succeed.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CoreRefusal(String);

/// The pull request a `pull_request` run belongs to. It becomes `github.event`.
#[derive(Clone, Serialize, Deserialize)]
pub struct PullRequestEvent {
    pub number: i32,
    pub title: String,
    pub head_ref: String,
    pub base_ref: String,
    pub base_sha: String,
}

/// The variables a run sees: its repository's, then inherited organization values that the
/// repository does not set. Admission and claiming both read them, so they agree.
pub(super) async fn run_variables(pool: &DbPool, repo: i64) -> Result<Vec<(String, String)>> {
    let variables = codeyard_db_actions::variables::for_run(pool, repo).await?;
    codeyard_db_actions::variables::validate_all(&variables)?;
    let mut variables: std::collections::BTreeMap<_, _> = variables.into_iter().collect();
    let mut bytes: usize = variables
        .iter()
        .map(|(name, value)| name.len() + value.len())
        .sum();
    for (name, value) in
        codeyard_db_orgs::organization_values::variables_for_run(pool, repo).await?
    {
        if variables.contains_key(&name) {
            continue;
        }
        if bytes + name.len() + value.len() > codeyard_actions::values::MAX_VARIABLE_BYTES {
            break;
        }
        bytes += name.len() + value.len();
        variables.insert(name, value);
    }
    Ok(variables.into_iter().collect())
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RunActor {
    pub id: i64,
    pub login: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ClaimedRun {
    pub run: CiRun,
    /// The core's canonical forge origin, which workflow URLs are built from on any runner.
    #[serde(default)]
    pub forge_origin: String,
    pub storage_node_id: Option<i64>,
    pub repository: Uuid,
    pub repository_name: String,
    pub owner: String,
    /// The owner's id, which `github.repository_owner_id` shows; 0 from a core that sent none.
    #[serde(default)]
    pub owner_id: i64,
    /// Login of the user who started the run; empty when no account did.
    pub actor: String,
    #[serde(default)]
    pub original_actor: Option<RunActor>,
    #[serde(default)]
    pub replay: Option<codeyard_actions::progress::Replay>,
    #[serde(default)]
    pub debug: bool,
    pub secrets: Vec<(String, String)>,
    #[serde(default)]
    pub variables: Vec<(String, String)>,
    #[serde(default)]
    pub environment_values: crate::backend::ci::environment_values::EnvironmentValues,
    #[serde(default)]
    pub pull_request: Option<PullRequestEvent>,
    pub storage_peer: Option<crate::backend::cluster::CorePeer>,
    #[serde(default)]
    pub storage_replicas: Vec<(i64, crate::backend::cluster::CorePeer)>,
    #[serde(default)]
    pub limits: RunLimits,
    #[serde(default)]
    pub runtime: crate::backend::ci::runtime::RuntimeCredentials,
    #[serde(default)]
    pub github_tokens: crate::backend::ci::tokens::GithubTokens,
    // Used only by a database worker; never sent to a remote worker.
    #[serde(skip)]
    pub(crate) store: Option<RepoStore>,
}

#[derive(Clone)]
pub enum RunQueue {
    Database {
        pool: DbPool,
        config: Arc<CiConfig>,
        secrets: Option<Arc<SecretEncryptor>>,
        context: Option<NodeContext>,
        notifications: Arc<NotificationBroadcaster>,
    },
    Remote {
        context: NodeContext,
        peers: Vec<crate::backend::cluster::CorePeer>,
        preferred: Arc<std::sync::Mutex<Option<String>>>,
    },
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Operation {
    Claim {
        mode: CiExecutionMode,
        platforms: Vec<String>,
    },
    Heartbeat {
        id: i64,
    },
    FinishJob {
        id: i64,
        job: String,
    },
    Progress {
        id: i64,
        generation: String,
        workflow: String,
        event: codeyard_actions::progress::Event,
    },
    JobState {
        id: i64,
        generation: String,
        workflow: String,
        state: codeyard_actions::progress::State,
    },
    AdmitJob {
        id: i64,
        generation: String,
        workflow: String,
        job: i64,
    },
    Storage {
        run_id: Uuid,
    },
    Logs {
        id: i64,
        sequence: i64,
        lines: Vec<LogLine>,
    },
    Finish {
        id: i64,
        status: String,
        message: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Request {
    pub worker: String,
    pub operation: Operation,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Reply {
    pub result: std::result::Result<serde_json::Value, String>,
}

/// The reply of a core node that cannot serve CI now: it is not an active member with its
/// own key, or it is busy. The caller then asks the next core.
pub(crate) const CORE_UNAVAILABLE: &str = "core node unavailable";

impl RunQueue {
    /// The service a database queue sends workflow webhooks through.
    fn events(&self) -> Option<crate::backend::ci::CiRuns> {
        let Self::Database {
            pool,
            config,
            context,
            notifications,
            ..
        } = self
        else {
            return None;
        };
        Some(crate::backend::ci::CiRuns::new(
            pool.clone(),
            config.clone(),
            Arc::new(tokio::sync::Notify::new()),
            context.clone(),
            notifications.clone(),
        ))
    }

    pub(super) async fn emit_workflow_run(&self, run: &CiRun, action: &str) {
        if let Some(ci) = self.events() {
            ci.emit_workflow_run(run, action).await;
        }
    }

    /// Fails runs whose worker stopped heartbeating. Each finished run emits its completion
    /// event, as a run finished by its worker does.
    pub(super) async fn fail_stale(&self, stale_seconds: i64) -> Result<()> {
        let Self::Database {
            pool,
            notifications,
            ..
        } = self
        else {
            return Ok(());
        };
        let (finished, created, jobs) =
            codeyard_db_actions::ci::fail_stale_ci_runs(pool, stale_seconds).await?;
        notifications.send_created(&created).await;
        if let Some(ci) = self.events() {
            for (run, job) in jobs {
                ci.emit_workflow_job(run, job, "completed").await;
            }
        }
        for run in &finished {
            self.emit_workflow_run(run, "completed").await;
        }
        Ok(())
    }

    pub fn store(&self, claim: &ClaimedRun) -> Result<RepoStore> {
        match self {
            Self::Database { .. } => claim
                .store
                .clone()
                .context("the claim has no repository store"),
            Self::Remote { context, .. } => {
                let peer = claim
                    .storage_peer
                    .as_ref()
                    .context("storage node unavailable")?;
                let id = claim.storage_node_id.context("storage node unavailable")?;
                let worker = claim
                    .run
                    .worker
                    .as_deref()
                    .context("the CI run has no worker")?;
                context.remember_peer(id, peer.clone());
                for (id, peer) in &claim.storage_replicas {
                    context.remember_peer(*id, peer.clone());
                }
                let mut context = context.clone();
                context.ci_replicas =
                    Arc::new(std::sync::RwLock::new(std::collections::HashMap::from([(
                        claim.repository,
                        claim.storage_replicas.iter().map(|(id, _)| *id).collect(),
                    )])));
                Ok(RepoStore::remote(claim.repository, id, &context)
                    .for_ci_run(claim.run.id, worker))
            }
        }
    }

    /// Checkout retries a remote worker's stale placement once, through its authenticated core.
    pub async fn checkout(
        &self,
        claim: &ClaimedRun,
        destination: &std::path::Path,
    ) -> Result<RepoStore> {
        let mut store = self.store(claim)?;
        let sha = claim
            .run
            .head_sha
            .as_deref()
            .context("the run has no commit")?;
        if let Err(error) = store.checkout(destination, sha).await {
            if let Self::Database { pool, context, .. } = self {
                let repository = codeyard_db_repos::repositories::records::get_repository_by_id(
                    pool,
                    claim.run.repo_id,
                )
                .await?
                .context("the repository no longer exists")?;
                store = repo_store(&repository, context.as_ref()).for_ci_run(
                    claim.run.id,
                    claim
                        .run
                        .worker
                        .as_deref()
                        .context("the CI run has no worker")?,
                );
            } else if let Self::Remote { context, .. } = self {
                let worker = claim
                    .run
                    .worker
                    .as_deref()
                    .context("the CI run has no worker")?;
                let local_worker = worker
                    .strip_prefix(&format!("{}/", context.key.public_hex()))
                    .context("the CI run belongs to another worker")?;
                let (node, peer): (i64, crate::backend::cluster::CorePeer) = self
                    .call(
                        local_worker,
                        Operation::Storage {
                            run_id: claim.run.run_id,
                        },
                        None,
                    )
                    .await?;
                context.remember_peer(node, peer);
                store = RepoStore::remote(claim.repository, node, context)
                    .for_ci_run(claim.run.id, worker);
            } else {
                return Err(error.into());
            }
            let _lock = codeyard_git::locks::lock(destination).await;
            if destination.exists() {
                tokio::fs::remove_dir_all(destination).await?;
            }
            drop(_lock);
            store.checkout(destination, sha).await?;
        }
        Ok(store)
    }

    pub async fn claim(
        &self,
        worker: &str,
        mode: CiExecutionMode,
        platforms: &[String],
    ) -> Result<Option<ClaimedRun>> {
        self.claim_with_stop(worker, mode, platforms, None).await
    }

    /// `claim` for a worker that may be shutting down. A remote claim that is still dialing when
    /// `stop` is set gives up without sending its request. Once the request is sent the claim is
    /// awaited: the core may already have committed a run for this worker, and only its answer
    /// says so.
    pub async fn claim_with_stop(
        &self,
        worker: &str,
        mode: CiExecutionMode,
        platforms: &[String],
        stop: Option<&mut watch::Receiver<bool>>,
    ) -> Result<Option<ClaimedRun>> {
        // The preparation's state runs to tens of kilobytes in a debug build. On the heap, the
        // futures that await a claim stay small, and so do the stack frames that poll them.
        let prepare = Box::pin(self.claim_prepared(worker, mode, platforms, stop));
        match self {
            Self::Database { pool, .. } => {
                codeyard_db_actions::ci::prepare_claim(pool, worker, prepare).await
            }
            Self::Remote { .. } => prepare.await,
        }
    }

    async fn claim_prepared(
        &self,
        worker: &str,
        mode: CiExecutionMode,
        platforms: &[String],
        stop: Option<&mut watch::Receiver<bool>>,
    ) -> Result<Option<ClaimedRun>> {
        match self {
            Self::Remote { .. } => {
                self.call(
                    worker,
                    Operation::Claim {
                        mode,
                        platforms: platforms.to_vec(),
                    },
                    stop,
                )
                .await
            }
            Self::Database {
                pool,
                config,
                secrets,
                context,
                ..
            } => {
                let trusted: Vec<_> = config.trusted_actor_ids.iter().copied().collect();
                let claimed = match context {
                    // Cluster worker ids start with the node's public key, then `/`.
                    Some(_) => {
                        let node_key = worker.split_once('/').map_or(worker, |(key, _)| key);
                        codeyard_db_actions::ci::claim_ci_run_on_node(
                            pool,
                            node_key,
                            worker,
                            mode.as_str(),
                            &trusted,
                            platforms,
                        )
                        .await?
                    }
                    // A database without cluster membership has no node rows to check.
                    None => {
                        codeyard_db_actions::ci::claim_ci_run(
                            pool,
                            worker,
                            mode.as_str(),
                            &trusted,
                            platforms,
                        )
                        .await?
                    }
                };
                let Some(mut run) = claimed else {
                    return Ok(None);
                };
                let mut load_error = "CI could not load the repository or secrets";
                let run_id = run.id;
                let loaded = async {
                    let repository =
                        codeyard_db_repos::repositories::records::get_repository_by_id(
                            pool,
                            run.repo_id,
                        )
                        .await?
                        .context("the repository no longer exists")?;
                    let uuid =
                        crate::backend::cluster::rpc::repository_uuid(&repository.disk_path)?;
                    let owner = codeyard_db_actions::ci::ci_repository_owner(pool, run.repo_id)
                        .await?
                        .unwrap_or_default();
                    let actor = match run.triggered_by_id {
                        Some(id) => codeyard_db_identity::users::accounts::get_user_by_id(pool, id)
                            .await?
                            .map(|user| user.username)
                            .unwrap_or_default(),
                        None => String::new(),
                    };
                    let original_actor =
                        if let Some(root) = run.workflow_run_id.filter(|root| *root != run.id) {
                            let source =
                                codeyard_db_actions::ci::get_ci_run_by_id(pool, run.repo_id, root)
                                    .await?
                                    .context("original workflow run is unavailable")?;
                            Some(match source.triggered_by_id {
                                Some(id) => {
                                    codeyard_db_identity::users::accounts::get_user_by_id(pool, id)
                                        .await?
                                        .map(|user| RunActor {
                                            id: user.id,
                                            login: user.username,
                                        })
                                        .unwrap_or(RunActor {
                                            id: 0,
                                            login: "ghost".into(),
                                        })
                                }
                                None => RunActor {
                                    id: 0,
                                    login: "ghost".into(),
                                },
                            })
                        } else {
                            run.triggered_by_id.map(|id| RunActor {
                                id,
                                login: actor.clone(),
                            })
                        };
                    let options = codeyard_db_actions::ci::ci_run_options(pool, run.id)
                        .await?
                        .unwrap_or_default();
                    let pull_request = run
                        .pull_request_event
                        .clone()
                        .map(serde_json::from_value)
                        .transpose()?;
                    let secrets_encryptor = secrets.as_deref();
                    let secrets = match secrets {
                        Some(encryptor) if !run.is_fork => {
                            let mut values =
                                codeyard_db_orgs::organization_values::secrets_for_run(
                                    pool,
                                    encryptor,
                                    run.repo_id,
                                )
                                .await?;
                            let repository =
                                codeyard_db_actions::secrets::decrypt_repository_secrets(
                                    pool,
                                    encryptor,
                                    run.repo_id,
                                )
                                .await?;
                            let names: std::collections::BTreeSet<_> = repository
                                .iter()
                                .map(|(name, _)| name.to_ascii_uppercase())
                                .collect();
                            values.retain(|(name, _)| !names.contains(name));
                            values.extend(repository);
                            values
                        }
                        None if !run.is_fork => {
                            ensure!(
                                !codeyard_db_actions::secrets::repository_has_secrets(
                                    pool,
                                    run.repo_id
                                )
                                .await?
                                    && codeyard_db_orgs::organization_values::workflow_values(
                                        pool,
                                        run.repo_id,
                                        codeyard_db_orgs::organization_values::Kind::Secret
                                    )
                                    .await?
                                    .is_empty(),
                                "repository secrets cannot be decrypted without SECRETS_MASTER_KEY"
                            );
                            Vec::new()
                        }
                        _ => Vec::new(),
                    };
                    // The whole set goes out in one RPC reply and into one secret file, so it
                    // is bounded before the run is handed out.
                    codeyard_db_actions::secrets::validate_workflow_secrets(&secrets)?;
                    let variables = run_variables(pool, run.repo_id).await?;
                    codeyard_db_actions::variables::validate_workflow_variables(&variables)?;
                    let primary = repository
                        .storage_node_id
                        .or_else(|| context.as_ref().map(|c| c.node_id));
                    let mut targets = Vec::new();
                    if let Some(primary) = primary {
                        let mut candidates = vec![primary];
                        if let Some(context) = context {
                            candidates.extend(
                                context
                                    .read_copies(uuid, primary)
                                    .await
                                    .into_iter()
                                    .map(|(id, _)| id),
                            );
                        }
                        for id in candidates {
                            let Some(node) = codeyard_db_cluster::cluster::get_node_by_id(pool, id)
                                .await?
                                .filter(|node| {
                                    node.status == "active"
                                        && node.roles.iter().any(|role| role == "storage")
                                })
                            else {
                                continue;
                            };
                            let Some(address) = node
                                .addr_hint
                                .as_deref()
                                .and_then(|hint| serde_json::from_str(hint).ok())
                            else {
                                continue;
                            };
                            targets.push((
                                id,
                                crate::backend::cluster::CorePeer {
                                    target: None,
                                    public_key: node.public_key,
                                    address,
                                },
                            ));
                        }
                        ensure!(!targets.is_empty(), "storage node unavailable");
                    }
                    let (storage_id, storage_peer) = match targets.first() {
                        Some((id, peer)) => (Some(*id), Some(peer.clone())),
                        None => (None, None),
                    };
                    let storage_replicas = targets.into_iter().skip(1).collect();
                    load_error = "CI could not run";
                    let store = repo_store(&repository, context.as_ref()).for_ci_run(
                        run.id,
                        run.worker.as_deref().context("the CI run has no worker")?,
                    );
                    let mut definitions = std::collections::BTreeMap::new();
                    for (path, sha) in run
                        .workflow_blobs
                        .as_object()
                        .context("the run has no workflow files")?
                    {
                        let bytes =
                            if let Some(content) = crate::backend::ci::reusable::content(sha) {
                                let called_id = sha
                                    .get("repository_id")
                                    .and_then(serde_json::Value::as_i64)
                                    .context("invalid called workflow repository")?;
                                let called =
                                    codeyard_db_repos::repositories::records::get_repository_by_id(
                                        pool, called_id,
                                    )
                                    .await?
                                    .context("called workflow repository unavailable")?;
                                crate::backend::ci::reusable::authorize(
                                    pool,
                                    &repository,
                                    run.triggered_by_id.context("run has no actor")?,
                                    &called,
                                )
                                .await?;
                                content.as_bytes().to_vec()
                            } else {
                                store
                                    .blob_by_id(sha.as_str().context("invalid workflow blob")?)
                                    .await?
                            };
                        definitions.insert(path.clone(), bytes);
                    }
                    let environment_values = crate::backend::ci::environment_values::load(
                        pool,
                        secrets_encryptor,
                        &run,
                        &definitions,
                    )
                    .await?;
                    let site = codeyard_db_actions::actions::get_actions_settings(pool).await?;
                    run.engine = Some(
                        codeyard_db_actions::actions::record_ci_engine(
                            pool,
                            run.id,
                            worker,
                            site.effective_engine(repository.actions_engine.as_deref()),
                        )
                        .await?,
                    );
                    let (repo_policy, org_policy) =
                        codeyard_db_actions::action_settings::effective(pool, run.repo_id).await?;
                    let github_tokens = crate::backend::ci::tokens::issue_selected(
                        pool,
                        &run,
                        &definitions,
                        codeyard_db_actions::action_settings::workflow_write(
                            &repo_policy,
                            &org_policy,
                            site.default_workflow_permissions == "permissive",
                        ),
                        options.replay.as_ref().map(|replay| &replay.0),
                    )
                    .await?;
                    let runtime = crate::backend::ci::runtime::issue(pool, &run, &site).await?;
                    if run.engine.as_deref() == Some("codeyard-actions") {
                        for workflow in run.workflow_path.split(',') {
                            codeyard_db_actions::action_jobs::save_state(
                                pool,
                                worker,
                                run.id,
                                &crate::backend::ci::runtime::token_hash(&runtime.token),
                                workflow,
                                &Default::default(),
                            )
                            .await?;
                        }
                    }
                    let limits = RunLimits::new(
                        &site,
                        &codeyard_db_actions::actions::get_repo_actions_settings(pool, run.repo_id)
                            .await?,
                    );
                    let store = Some(store);
                    Ok::<_, anyhow::Error>(ClaimedRun {
                        run: run.clone(),
                        storage_node_id: storage_id,
                        repository: uuid,
                        owner_id: repository.owner_id(),
                        repository_name: repository.name,
                        owner,
                        actor,
                        original_actor,
                        forge_origin: codeyard_model::http_config::link_origin(),
                        replay: options.replay.map(|data| data.0),
                        debug: options.debug,
                        secrets,
                        variables,
                        environment_values,
                        pull_request,
                        storage_peer,
                        storage_replicas,
                        limits,
                        runtime,
                        github_tokens,
                        store,
                    })
                };
                // Preparing can wait on remote storage for longer than the stale-run sweep
                // allows, so the lease is renewed meanwhile. A lease taken back stops it.
                let renew = async {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
                    tick.tick().await;
                    loop {
                        tick.tick().await;
                        if matches!(self.heartbeat(worker, run_id).await, Ok(None | Some(true))) {
                            return;
                        }
                    }
                };
                let prepare_and_announce = async {
                    let claim = loaded.await?;
                    self.emit_workflow_run(&claim.run, "in_progress").await;
                    ensure!(
                        self.heartbeat(worker, run_id)
                            .await?
                            .is_some_and(|cancelled| !cancelled),
                        "the CI run was taken back while it was prepared"
                    );
                    Ok(claim)
                };
                let loaded = tokio::select! {
                    loaded = tokio::time::timeout(std::time::Duration::from_secs(60), prepare_and_announce) => loaded.unwrap_or_else(|_| Err(anyhow::anyhow!("CI claim preparation timed out"))),
                    () = renew => Err(anyhow::anyhow!("the CI run was taken back while it was prepared")),
                };
                if loaded.is_err() {
                    // A run cancelled while it was prepared ends as cancelled, not as a failure.
                    let (status, message) = match self.heartbeat(worker, run_id).await {
                        Ok(Some(true)) => ("cancelled", "Cancelled"),
                        _ => ("failed", load_error),
                    };
                    self.finish(worker, run_id, status, Some(message)).await?;
                }
                loaded.map(Some)
            }
        }
    }

    pub async fn progress(
        &self,
        worker: &str,
        id: i64,
        generation: &str,
        workflow: &str,
        event: &codeyard_actions::progress::Event,
    ) -> Result<i64> {
        ensure!(
            serde_json::to_vec(event)?.len() <= codeyard_actions::progress::MAX_EVENT_BYTES,
            "progress exceeds native limits"
        );
        match self {
            Self::Database { pool, .. } => {
                use codeyard_actions::progress::Update;
                let job = codeyard_db_actions::action_jobs::apply(
                    pool, worker, id, generation, workflow, event,
                )
                .await?;
                let action = match event.update {
                    Update::Queued { .. } => Some("queued"),
                    Update::Started => Some("in_progress"),
                    Update::Finished { .. } => Some("completed"),
                    _ => None,
                };
                if job.transitioned {
                    if let (Some(action), Some(ci)) = (action, self.events()) {
                        ci.emit_workflow_job(id, job.id, action).await;
                    }
                }
                Ok(job.id)
            }
            Self::Remote { .. } => {
                self.call(
                    worker,
                    Operation::Progress {
                        id,
                        generation: generation.into(),
                        workflow: workflow.into(),
                        event: event.clone(),
                    },
                    None,
                )
                .await
            }
        }
    }
    pub async fn job_state(
        &self,
        worker: &str,
        id: i64,
        generation: &str,
        workflow: &str,
        state: &codeyard_actions::progress::State,
    ) -> Result<()> {
        state.validate()?;
        match self {
            Self::Database { pool, .. } => {
                codeyard_db_actions::action_jobs::save_state(
                    pool, worker, id, generation, workflow, state,
                )
                .await
            }
            Self::Remote { .. } => {
                self.call(
                    worker,
                    Operation::JobState {
                        id,
                        generation: generation.into(),
                        workflow: workflow.into(),
                        state: state.clone(),
                    },
                    None,
                )
                .await
            }
        }
    }

    /// Whether a job waiting for its concurrency group may start: `Some(true)` once it holds
    /// the group, `Some(false)` once a newer job cancelled it, `None` while it waits.
    pub async fn admit_job(
        &self,
        worker: &str,
        id: i64,
        generation: &str,
        workflow: &str,
        job: i64,
    ) -> Result<Option<bool>> {
        match self {
            Self::Database { pool, .. } => {
                codeyard_db_actions::action_jobs::admit(pool, worker, id, generation, workflow, job)
                    .await
            }
            Self::Remote { .. } => {
                self.call(
                    worker,
                    Operation::AdmitJob {
                        id,
                        generation: generation.into(),
                        workflow: workflow.into(),
                        job,
                    },
                    None,
                )
                .await
            }
        }
    }

    pub async fn finish_job(&self, worker: &str, id: i64, job: &str) -> Result<()> {
        ensure!(job.len() <= 1024, "invalid job id");
        match self {
            Self::Database { pool, .. } => {
                codeyard_db_actions::ci_tokens::finish_job(pool, id, worker, job).await
            }
            Self::Remote { .. } => {
                self.call(
                    worker,
                    Operation::FinishJob {
                        id,
                        job: job.into(),
                    },
                    None,
                )
                .await
            }
        }
    }

    pub async fn heartbeat(&self, worker: &str, id: i64) -> Result<Option<bool>> {
        match self {
            Self::Database { pool, context, .. } => match context {
                Some(_) => {
                    codeyard_db_actions::ci::heartbeat_ci_run_on_node(
                        pool,
                        id,
                        worker,
                        worker.split_once('/').map_or(worker, |(key, _)| key),
                    )
                    .await
                }
                None => codeyard_db_actions::ci::heartbeat_ci_run(pool, id, worker).await,
            },
            Self::Remote { .. } => self.call(worker, Operation::Heartbeat { id }, None).await,
        }
    }

    /// Returns false once the run's stored log is closed.
    pub async fn logs(
        &self,
        worker: &str,
        id: i64,
        sequence: i64,
        lines: &[LogLine],
    ) -> Result<bool> {
        let lines = prepare_logs(lines)?;
        match self {
            Self::Database { pool, .. } => {
                codeyard_db_actions::ci::append_ci_logs(pool, id, worker, sequence, &lines).await
            }
            Self::Remote { .. } => {
                self.call(
                    worker,
                    Operation::Logs {
                        id,
                        sequence,
                        lines,
                    },
                    None,
                )
                .await
            }
        }
    }

    pub async fn finish(
        &self,
        worker: &str,
        id: i64,
        status: &str,
        message: Option<&str>,
    ) -> Result<bool> {
        ensure!(
            matches!(status, "success" | "failed" | "cancelled"),
            "invalid CI terminal status"
        );
        ensure!(
            message.is_none_or(|m| m.len() <= 16 * 1024),
            "CI result exceeds ingestion limits"
        );
        match self {
            Self::Database {
                pool,
                notifications,
                ..
            } => {
                let finished =
                    codeyard_db_actions::ci::finish_ci_run(pool, id, worker, status, message)
                        .await?;
                if let Some((run, created, jobs)) = &finished {
                    notifications.send_created(created).await;
                    if let Some(ci) = self.events() {
                        for job in jobs {
                            ci.emit_workflow_job(run.id, *job, "completed").await;
                        }
                    }
                    self.emit_workflow_run(run, "completed").await;
                }
                Ok(finished.is_some())
            }
            Self::Remote { .. } => {
                self.call(
                    worker,
                    Operation::Finish {
                        id,
                        status: status.into(),
                        message: message.map(str::to_owned),
                    },
                    None,
                )
                .await
            }
        }
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        worker: &str,
        operation: Operation,
        mut stop: Option<&mut watch::Receiver<bool>>,
    ) -> Result<T> {
        let Self::Remote {
            context,
            peers,
            preferred,
        } = self
        else {
            unreachable!()
        };
        let header = serde_json::to_vec(&Request {
            worker: worker.into(),
            operation,
        })?;
        let mut last = anyhow::anyhow!("core node unavailable");
        let first = preferred.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let ordered = peers
            .iter()
            .filter(|peer| Some(&peer.public_key) == first.as_ref())
            .chain(
                peers
                    .iter()
                    .filter(|peer| Some(&peer.public_key) != first.as_ref()),
            );
        for peer in ordered {
            let node = peer.node()?;
            // dial_node bounds each attempt itself. An outer timeout here would cut the
            // fallback short, so the other addresses and iroh would never be tried. A stop
            // ends only the dial: once the request is written, its reply is awaited below.
            let dial = async {
                let address = peer.resolved_address().await?;
                crate::backend::cluster::dial_node(
                    &context.iroh,
                    node,
                    &address,
                    crate::backend::cluster::ALPN_EVENTS,
                )
                .await
            };
            let dialed = match stop.as_deref_mut() {
                Some(stop) => {
                    // The stop wins a tie, and is read again once the dial is ready, so a
                    // stopping worker never writes a request that could claim a run.
                    let dialed = tokio::select! {
                        biased;
                        _ = stop.wait_for(|stopped| *stopped) => None,
                        dialed = dial => Some(dialed),
                    };
                    match dialed {
                        Some(dialed) if !*stop.borrow() => dialed,
                        _ => {
                            return Err(anyhow::anyhow!(
                                "CI worker stopped before reaching the core"
                            ));
                        }
                    }
                }
                None => dial.await,
            };
            let Ok(mut link) = dialed else {
                continue;
            };
            let result = tokio::time::timeout(std::time::Duration::from_secs(100), async {
                crate::backend::cluster::rpc::write_frame(
                    &mut link.writer,
                    &header,
                    codeyard_db_core::limits::MAX_JSON + 64 * 1024,
                )
                .await?;
                link.finish().await?;
                let reply: Reply = serde_json::from_slice(
                    &crate::backend::cluster::rpc::read_frame(
                        &mut link.reader,
                        codeyard_db_core::limits::MAX_JSON,
                    )
                    .await?,
                )?;
                Ok::<_, anyhow::Error>(reply)
            })
            .await;
            link.close(0, b"CI request complete");
            match result {
                Ok(Ok(reply)) => match reply.result {
                    Ok(value) => {
                        let value = serde_json::from_value(value)?;
                        *preferred.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(peer.public_key.clone());
                        return Ok(value);
                    }
                    Err(error) if error == CORE_UNAVAILABLE => {
                        last = anyhow::anyhow!(CORE_UNAVAILABLE)
                    }
                    Err(error) => return Err(CoreRefusal(error).into()),
                },
                Ok(Err(error)) => {
                    last = error;
                }
                Err(_) => {
                    last = anyhow::anyhow!("core node request timed out");
                }
            }
        }
        Err(last)
    }
}

use codeyard_db_core::ci_logs::prepare_logs;

//! A CI worker: claims queued runs and executes their workflows with `act`.
use super::config::{podman_socket_path, CiExecutionMode};
use super::process::{self, Stopped};
use super::queue::{ClaimedRun, CoreRefusal, LogLine, RunQueue};
use super::runner_labels::labels_from_value;
use super::workers::CiWorkers;
use crate::backend::db::{self, models::CiRun};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tracing::warn;

const IDLE_POLL: Duration = Duration::from_secs(3);
const HEARTBEAT: Duration = Duration::from_secs(10);
/// Without an acknowledged heartbeat for this long, a run is cancelled before the core's stale
/// cutoff. The margin covers claim preparation before the first heartbeat, and cleanup.
const LOST_HEARTBEAT_CANCEL: Duration = Duration::from_secs(45);
/// How long a progress or completion update may keep retrying before its workflow fails.
const DELIVERY_RETRY: Duration = Duration::from_secs(60);
const RUN_LABEL_KEY: &str = "com.codeyard.ci.run";
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);
const CLEANUP_RETRY_TIMEOUT: Duration = Duration::from_secs(30);

/// A running run without a heartbeat for this long has lost its worker.
const STALE_AFTER_SECONDS: i64 = 90;

/// The longest wait between attempts to record a final state.
const FINISH_RETRY_MAX: Duration = Duration::from_secs(60);

/// How long a claim still in flight at shutdown may run, for a database or a remote queue.
/// A claim that has not returned by then is dropped, so shutdown stays prompt.
const SHUTDOWN_CLAIM_GRACE: Duration = Duration::from_secs(2);

/// Output lines reach the core in batches of at most this many lines (the core's limit per
/// call) or bytes. A waiting batch goes out after `LOG_BATCH_INTERVAL` at the latest, and the
/// command may print `LOG_QUEUE_LINES` lines ahead of the sender. A stalled remote sender
/// gets one short wait before overflow becomes lossy.
const LOG_BATCH_LINES: usize = 64;
const LOG_BATCH_BYTES: usize = 64 * 1024;
const LOG_BATCH_INTERVAL: Duration = Duration::from_millis(250);
const LOG_QUEUE_LINES: usize = 256;
const LOG_FLUSH_GRACE: Duration = Duration::from_secs(1);

/// Claims and executes runs until `stop` is set. With `recover`, abandoned containers are
/// removed before the first claim.
pub(super) async fn run(
    service: CiWorkers,
    worker: String,
    mut stop: watch::Receiver<bool>,
    mut recover: bool,
) {
    let mut backoff = IDLE_POLL;
    while !*stop.borrow() {
        if recover {
            let recovered =
                tokio::time::timeout(CLEANUP_RETRY_TIMEOUT, prepare_slot(&service.cleanup_dir));
            tokio::select! {
                result = recovered => match result {
                    Ok(Ok(())) => recover = false,
                    _ => {
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {}
                            _ = stop.changed() => {}
                        }
                        backoff = (backoff * 2).min(FINISH_RETRY_MAX);
                        continue;
                    }
                },
                _ = stop.changed() => break,
            }
        }
        // A claim may commit a run as shutdown starts. Its grace lets that run reach `execute`,
        // which cancels and finishes it, instead of leaving it running until the stale sweep.
        // A remote claim still dialing when shutdown starts gives up at once, before its request
        // is sent. Once the request is sent, the claim is awaited under the same grace, because
        // the core may already have committed the run for this worker.
        // The claim waits on its own receiver, so the waits below still see the stop signal.
        let mut stopping = stop.clone();
        let mut claim_stop = stop.clone();
        // A database worker claims only while its node is an active runner with its own
        // key. A revoked or re-keyed node must not take runs it cannot finish.
        let claim = async {
            if let RunQueue::Database {
                pool,
                context: Some(context),
                ..
            } = &service.queue
            {
                ensure!(
                    db::cluster::get_node_by_id(pool, context.node_id)
                        .await?
                        .is_some_and(|node| {
                            node.public_key == context.key.public_hex()
                                && node.status == "active"
                                && node.roles.iter().any(|role| role == "runner")
                        }),
                    "not an active runner"
                );
            }
            service
                .queue
                .claim_with_stop(&worker, service.config.mode, Some(&mut claim_stop))
                .await
        };
        tokio::pin!(claim);
        let outcome = tokio::select! {
            result = &mut claim => result,
            _ = stopping.changed() => {
                match tokio::time::timeout(SHUTDOWN_CLAIM_GRACE, &mut claim).await {
                    Ok(result) => result,
                    Err(_) => break,
                }
            }
        };
        match outcome {
            Ok(Some(run)) => {
                backoff = IDLE_POLL;
                let run_id = run.run.run_id;
                if !execute(&service, &worker, run, stop.clone()).await {
                    warn!(%worker, run = %run_id, "CI worker slot waiting for recovery");
                    recover = true;
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = stop.changed() => {}
                    }
                    backoff = (backoff * 2).min(FINISH_RETRY_MAX);
                }
            }
            Ok(None) => {
                backoff = IDLE_POLL;
                tokio::select! {
                    _ = service.queued.notified() => {}
                    _ = tokio::time::sleep(IDLE_POLL) => {}
                    _ = stop.changed() => {}
                }
            }
            Err(error) => {
                // A claim that ended because the worker is stopping is not a failure.
                if *stop.borrow() {
                    break;
                }
                if error.to_string() == "not an active runner" {
                    service.request_stop();
                    break;
                }
                warn!(%error, "CI worker could not claim a run");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = stop.changed() => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

/// Marks runs failed whose worker (on any node) stopped sending heartbeats.
pub(super) async fn fail_stale_runs(queue: RunQueue, mut stop: watch::Receiver<bool>) {
    while !*stop.borrow() {
        if let Err(error) = queue.fail_stale(STALE_AFTER_SECONDS).await {
            warn!(%error, "could not check for abandoned CI runs");
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            _ = stop.changed() => {}
        }
    }
}

async fn execute(
    service: &CiWorkers,
    worker: &str,
    claim: ClaimedRun,
    mut stop: watch::Receiver<bool>,
) -> bool {
    let run = &claim.run;
    // Publish the locked cleanup record before act can create anything in the host engine.
    let ownership = {
        let socket = if service.config.mode == CiExecutionMode::Podman {
            service.config.podman_socket.as_str()
        } else {
            ""
        };
        let prepared = (|| -> Result<_> {
            let root = match &service.config.process_cgroup_root {
                Some(root) => Some(std::fs::canonicalize(root)?),
                None if service.config.mode == CiExecutionMode::Local => Some(codeyard_actions::process::Cgroup::current_root()?),
                None => None,
            };
            ContainerOwnership::create(&service.cleanup_dir, socket, run.run_id, root.as_deref())
                .context("Unable to contain CI processes; configure a writable delegated CI_PROCESS_CGROUP_ROOT")
        })();
        match prepared {
            Ok(ownership) => ownership,
            Err(error) => {
                warn!(%error, "could not persist CI execution ownership");
                record_result(
                    &service.queue,
                    worker,
                    run,
                    "failed",
                    Some("CI execution recovery is unavailable"),
                    &mut stop,
                )
                .await;
                return false;
            }
        }
    };
    // A run handed back by a retried claim may have been cancelled since the first claim.
    let (cancel_tx, cancel) = watch::channel(run.cancel_requested || *stop.borrow());
    let (force_tx, force) = watch::channel(false);
    let (ready_tx, mut ready) = watch::channel(false);
    let mut fencing_signal = force.clone();
    let mut heartbeat_stop = stop.clone();
    // Set when the core stopped acknowledging heartbeats, so the run ends as failed, not cancelled.
    let lost = AtomicBool::new(false);
    let heartbeat = async {
        let mut shutdown = false;
        // The claim refreshed the lease just before; the first heartbeat goes out at once.
        let mut wait = Duration::ZERO;
        let mut last_ack = Instant::now();
        loop {
            // The core marks the run stale after STALE_AFTER_SECONDS without a heartbeat. Stop the
            // work before then, however long failed attempts take, so another run cannot take its
            // concurrency group while this one still runs.
            let lease = tokio::time::sleep_until((last_ack + LOST_HEARTBEAT_CANCEL).into());
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = heartbeat_stop.changed(), if !shutdown => {
                    shutdown = true;
                    let _ = cancel_tx.send(true);
                    continue;
                }
                _ = lease, if !lost.load(Ordering::Relaxed) => {
                    lost.store(true, Ordering::Relaxed);
                    warn!("CI heartbeat unacknowledged; fencing the run");
                    let _ = force_tx.send(true);
                    let _ = cancel_tx.send(true);
                    continue;
                }
            }
            wait = HEARTBEAT;
            let lease = tokio::time::sleep_until((last_ack + LOST_HEARTBEAT_CANCEL).into());
            let request_started = Instant::now();
            let reply = tokio::select! {
                reply = service.queue.heartbeat(worker, run.id) => reply,
                _ = heartbeat_stop.changed(), if !shutdown => {
                    shutdown = true;
                    let _ = cancel_tx.send(true);
                    continue;
                }
                _ = lease, if !lost.load(Ordering::Relaxed) => {
                    lost.store(true, Ordering::Relaxed);
                    warn!("CI heartbeat unacknowledged; fencing the run");
                    let _ = force_tx.send(true);
                    let _ = cancel_tx.send(true);
                    continue;
                }
            };
            match reply {
                Ok(Some(false)) => {
                    last_ack = request_started;
                    let _ = ready_tx.send(true);
                }
                Ok(Some(true)) => {
                    last_ack = request_started;
                    let _ = cancel_tx.send(true);
                }
                Ok(None) => { let _ = force_tx.send(true); let _ = cancel_tx.send(true); }
                Err(error) => {
                    if error.to_string().contains("not an active runner") {
                        let _ = force_tx.send(true);
                        let _ = cancel_tx.send(true);
                        service.request_stop();
                    }
                    warn!(%error, "CI heartbeat failed");
                }
            }
        }
    };
    let fencing = async {
        let _ = fencing_signal.wait_for(|fenced| *fenced).await;
        loop {
            match tokio::time::timeout(CLEANUP_RETRY_TIMEOUT, ownership.fence()).await {
                Ok(Ok(())) => {}
                _ => warn!(run = %run.run_id, "CI execution fencing will retry"),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };
    let work = async {
        let (cleaned, cleanup) = oneshot::channel();
        let mut workspace = None;
        let result = async {
            // A delayed claim must renew its live authority before any workflow process starts.
            tokio::select! {
                biased;
                _ = process::cancelled(cancel.clone()) => return Err(Stopped::Cancelled.into()),
                _ = ready.wait_for(|ready| *ready) => {}
            }
            run_workflows(service, worker, &claim, cancel, force, cleaned, &mut workspace, ownership.group.as_ref()).await
        }.await;
        let shutting_down = *stop.borrow();
        let (status, message) = match &result {
            Ok(true) => ("success", None),
            Ok(false) => ("failed", Some("A workflow job failed".to_owned())),
            Err(error) => match error.downcast_ref::<Stopped>() {
                Some(Stopped::Cancelled) if shutting_down => (
                    "failed",
                    Some("The CI worker shut down during the run".to_owned()),
                ),
                Some(Stopped::Cancelled) if lost.load(Ordering::Relaxed) => (
                    "failed",
                    Some("The CI worker lost contact with the server".to_owned()),
                ),
                Some(Stopped::Cancelled) => ("cancelled", Some("Cancelled".to_owned())),
                Some(Stopped::TimedOut) => {
                    ("failed", Some("The run exceeded its time limit".to_owned()))
                }
                None => {
                    // The cause may hold paths or credentials, so only the server log gets it.
                    warn!(error = %format!("{error:#}"), run = %run.run_id, "CI could not run");
                    let message = "CI could not run".to_owned();
                    let _ = send_log_lines(
                        service,
                        worker,
                        run,
                        &[LogLine {
                            stage: "setup".into(),
                            line: message.clone(),
                            level: "error".into(),
                            ..Default::default()
                        }],
                    )
                    .await;
                    ("failed", Some(message))
                }
            },
        };
        // A live container is still executing this run. Retain its workflow concurrency
        // lease and heartbeat until recovery confirms removal, not merely for this slot.
        let mut delay = Duration::from_secs(1);
        loop {
            match tokio::time::timeout(CLEANUP_RETRY_TIMEOUT, ownership.remove_containers()).await {
                Ok(Ok(())) => break,
                _ => warn!(run = %run.run_id, "CI container removal pending; retaining the run lease"),
            }
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = stop.wait_for(|stopped| *stopped) => return false,
            }
            delay = (delay * 2).min(FINISH_RETRY_MAX);
        }
        // No process or container can still access this checkout when removal starts.
        if let Some(workspace) = &mut workspace { workspace.removable = true; }
        drop(workspace);
        record_result(
            &service.queue,
            worker,
            run,
            status,
            message.as_deref(),
            &mut stop,
        )
        .await;
        // Recording the result must not wait for the blocking directory cleanup. Shutdown
        // retains that cleanup even when the blocking pool is busy.
        let _ = cleanup.await;
        if let Err(error) = ownership.remove_workspace().await {
            warn!(%error, run = %run.run_id, "could not remove CI workspace; recovery will retry");
            return false;
        }
        true
    };
    // Cleanup and result retries are still part of the running run.
    tokio::select! {
        reuse_slot = work => reuse_slot,
        _ = heartbeat => unreachable!("heartbeat loop never ends"),
        _ = fencing => unreachable!("fencing loop never ends"),
    }
}

/// Sends the final state of a run, and keeps trying until it is recorded or the worker stops.
/// Finishing is idempotent (the first final state wins), so a result whose reply was lost is
/// simply sent again. The slot claims nothing while its result is unrecorded: a claim would
/// hand back the run it still holds, and a job that already ran would run again.
async fn record_result(
    queue: &RunQueue,
    worker: &str,
    run: &CiRun,
    status: &str,
    message: Option<&str>,
    stop: &mut watch::Receiver<bool>,
) {
    let mut delay = Duration::from_secs(1);
    loop {
        match queue.finish(worker, run.id, status, message).await {
            Ok(_) => return,
            Err(error) => warn!(%error, run = %run.run_id, "could not record CI result"),
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = stop.wait_for(|stopped| *stopped) => return,
        }
        delay = (delay * 2).min(FINISH_RETRY_MAX);
    }
}

/// Retries a progress or completion update that failed in transit. The output callback waits for
/// each update in order, so a brief core outage does not fail a workflow that is still running.
/// A refusal from the core is final. A retried annotation whose reply was lost is not stored twice.
async fn deliver<T, Fut>(mut send: impl FnMut() -> Fut) -> Result<T>
where
    Fut: std::future::Future<Output = Result<T>>,
{
    let deadline = tokio::time::Instant::now() + DELIVERY_RETRY;
    let mut backoff = Duration::from_millis(250);
    loop {
        match send().await {
            Ok(value) => return Ok(value),
            Err(error)
                if !error.is::<CoreRefusal>()
                    && tokio::time::Instant::now() + backoff < deadline =>
            {
                warn!(%error, "CI update delivery failed; retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Clones the repository at the run's commit and runs each workflow in turn.
/// Returns whether every workflow succeeded.
async fn run_workflows(
    service: &CiWorkers,
    worker: &str,
    claim: &ClaimedRun,
    cancel: watch::Receiver<bool>,
    force: watch::Receiver<bool>,
    cleaned: oneshot::Sender<()>,
    workspace_owner: &mut Option<Workspace>,
    scope: Option<&codeyard_actions::process::Cgroup>,
) -> Result<bool> {
    let timeout = claim
        .limits
        .timeout_seconds
        .unwrap_or(service.config.run_timeout_seconds);
    let deadline = Instant::now() + Duration::from_secs(timeout.into());
    let run = &claim.run;
    let masker = credential_masker(claim);
    // A run still preparing when shutdown starts stops here, not after the checkout, and
    // the checkout counts against the run's time limit.
    let workspace_directory = service
        .cleanup_dir
        .join(format!("{}.workspace", run.run_id));
    let workspace = tokio::select! {
        prepared = Workspace::prepare(&service.queue, claim, &workspace_directory, service.cleanups.clone(), cleaned) => prepared?,
        _ = process::cancelled(cancel.clone()) => return Err(Stopped::Cancelled.into()),
        _ = tokio::time::sleep(deadline.saturating_duration_since(Instant::now())) => {
            return Err(Stopped::TimedOut.into());
        }
    };
    *workspace_owner = Some(workspace);
    let workspace = workspace_owner.as_ref().unwrap();
    let native = run.engine.as_deref() == Some("codeyard-actions");
    ensure!(
        claim.replay.is_none() || native,
        "Partial re-runs require the codeyard-actions engine"
    );
    let secret_file = if native {
        workspace.write_secrets_for(&claim.secrets, false)?
    } else {
        workspace.write_github_secrets(&claim.secrets, &claim.github_tokens.act)?
    };
    db::variables::validate_workflow_variables(&claim.variables)?;
    let variable_file = workspace.write_values("variables.env", &claim.variables, !native)?;
    workspace.write_event(claim)?;
    if !native && claim.github_tokens.act_limited {
        send_log_lines(service, worker, run, &[LogLine { stage: "setup".into(), line: "act runs give every job the same token; use the codeyard-actions engine for per-job permissions".into(), level: "warn".into(), ..Default::default() }]).await?;
    }
    let podman_label =
        (service.config.mode == CiExecutionMode::Podman).then(|| run_container_label(run.run_id));
    let mut log_open = true;
    let mut all_labels = BTreeSet::new();
    for workflow in run
        .workflow_blobs
        .as_object()
        .context("run has no workflow files")?
        .keys()
    {
        let definition = workspace.definition(workflow);
        let preparing = async {
            let permit = super::workflows::PARSING_SLOTS.acquire().await?;
            let path = definition.clone();
            let config = service.config.clone();
            let run_id = run.run_id;
            let limits = claim.limits.container_options();
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                prepare_workflow_for_engine(&path, &config, run_id, native, &limits)
            })
            .await?
        };
        let labels = tokio::select! {
            prepared = preparing => prepared?,
            _ = process::cancelled(cancel.clone()) => return Err(Stopped::Cancelled.into()),
            _ = tokio::time::sleep(deadline.saturating_duration_since(Instant::now())) => return Err(Stopped::TimedOut.into()),
        };
        all_labels.extend(labels);
    }
    let mut runtime_env = Vec::new();
    if !native && claim.github_tokens.oidc_jobs.contains("act") {
        runtime_env.extend([
            ("ACTIONS_ID_TOKEN_REQUEST_URL".to_owned(), claim.github_tokens.oidc_url.clone()),
            ("ACTIONS_ID_TOKEN_REQUEST_TOKEN".to_owned(), claim.github_tokens.act.clone()),
        ]);
    }
    if !claim.runtime.token.is_empty() {
        runtime_env.extend([
            ("ACTIONS_RUNTIME_TOKEN".to_owned(), claim.runtime.token.clone()),
            ("ACTIONS_RESULTS_URL".to_owned(), claim.runtime.results_url.clone()),
            ("ACTIONS_CACHE_URL".to_owned(), claim.runtime.results_url.clone()),
            ("ACTIONS_CACHE_SERVICE_V2".to_owned(), "True".to_owned()),
        ]);
    }
    let runtime_file = workspace.write_values("runtime.env", &runtime_env, !native)?;
    for workflow in run.workflow_path.split(',').filter(|path| !path.is_empty()) {
        workspace.restore_called_workflows(run)?;
        let definition = workspace.definition(workflow);
        let mut command = act_command(
            service,
            claim,
            workspace,
            &definition,
            &all_labels,
            secret_file.as_deref(),
            podman_label.as_deref(),
        );
        if let Some(file) = &runtime_file {
            command.arg("--env-file").arg(file);
        }
        if let Some(file) = &variable_file {
            command.arg("--var-file").arg(file);
        }
        if let Some(path) =
            workspace.write_environment_values(workflow, &claim.environment_values, run.is_fork)?
        {
            command.arg("--environment-values-file").arg(path);
        }
        let mut concurrency_dir = None;
        if native {
            if let Some(replay) = &claim.replay {
                ensure!(
                    !run.workflow_path.contains(','),
                    "partial replay requires one workflow"
                );
                command
                    .arg("--replay-file")
                    .arg(workspace.write_replay(workflow, replay)?);
            }
            if let Some(tokens) = claim.github_tokens.workflows.get(workflow) {
                let path = workspace.write_job_tokens(workflow, tokens, &claim.github_tokens)?;
                command
                    .arg("--job-token-file")
                    .arg(path)
                    .arg("--job-state-file")
                    .arg(workspace.job_state_path(workflow));
                let directory = workspace.concurrency_dir(workflow)?;
                command.arg("--concurrency-dir").arg(&directory);
                concurrency_dir = Some(directory);
            }
        }
        if let Some(scope) = scope {
            scope.attach(&mut command)?;
            if native { command.arg("--process-root").arg(scope.path()); }
        }
        let completion_prefix = format!(
            "::codeyard-job-completed::{}:",
            claim.github_tokens.completion_secret
        );
        let progress_prefix = format!(
            "::codeyard-progress::{}:",
            claim.github_tokens.completion_secret
        );
        let generation = super::runtime::token_hash(&claim.runtime.token);
        let records = Arc::new(Mutex::new(BTreeMap::<(String, u32), i64>::new()));
        let admissions = Arc::new(Mutex::new(BTreeMap::<(String, u32), i64>::new()));
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), Stopped::TimedOut);
        let stage = workflow.to_owned();
        let (lines_tx, lines_rx) = mpsc::channel(LOG_QUEUE_LINES);
        let stalled = Arc::new(AtomicBool::new(false));
        let remote = matches!(&service.queue, RunQueue::Remote { .. });
        let running = async {
            let admitting = async {
                match &concurrency_dir {
                    Some(directory) => {
                        answer_admissions(
                            service,
                            worker,
                            run.id,
                            &generation,
                            workflow,
                            &admissions,
                            directory,
                        )
                        .await
                    }
                    None => std::future::pending().await,
                }
            };
            let grace = if native { codeyard_actions::runner::SHUTDOWN_GRACE } else { Duration::from_secs(3) };
            let output = process::run_with_grace(command, remaining, grace, scope, force.clone(), cancel.clone(), |line, stderr| {
                let completed = if native && !claim.github_tokens.completion_secret.is_empty() {
                    line.strip_prefix(&completion_prefix)
                        .map(|job| format!("{workflow}:{job}"))
                } else {
                    None
                };
                let progress = if native && !claim.github_tokens.completion_secret.is_empty() {
                    line.strip_prefix(&progress_prefix)
                        .map(serde_json::from_str::<codeyard_actions::progress::Event>)
                } else {
                    None
                };
                let level = if stderr { log_level(&line) } else { "info" };
                let mut line = LogLine {
                    stage: stage.clone(),
                    line: if progress.is_none() && completed.is_none() { masker.mask(&line) } else { String::new() },
                    level: level.to_owned(),
                    ..Default::default()
                };
                let lines_tx = lines_tx.clone();
                let stalled = stalled.clone();
                let records = records.clone();
                let admissions = admissions.clone();
                let generation = generation.clone();
                let stage = stage.clone();
                let masker = masker.clone();
                async move {
                    if let Some(job) = completed {
                        return deliver(|| service.queue.finish_job(worker, run.id, &job)).await;
                    }
                    if let Some(event) = progress {
                        let event = event.context("invalid authenticated runner progress")?;
                        let key = (event.job.clone(), event.variant);
                        if let codeyard_actions::progress::Update::Log { step, text } =
                            &event.update
                        {
                            let id = records
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .get(&key)
                                .copied()
                                .context("output precedes its job observation")?;
                            line.line = masker.mask(text);
                            line.job_id = Some(id);
                            line.step_number = step.map(i32::try_from).transpose()?;
                        } else {
                            let id = deliver(|| {
                                service
                                    .queue
                                    .progress(worker, run.id, &generation, &stage, &event)
                            })
                            .await?;
                            if matches!(
                                event.update,
                                codeyard_actions::progress::Update::Concurrency { .. }
                            ) {
                                admissions
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .insert(key.clone(), id);
                            }
                            if matches!(event.update, codeyard_actions::progress::Update::Finished { .. }) {
                                admissions.lock().unwrap_or_else(|error| error.into_inner()).remove(&key);
                            }
                            records
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .insert(key, id);
                            return Ok(());
                        }
                    }
                    enqueue_log_line(&lines_tx, line, remote, &stalled).await
                }
            });
            let status = tokio::select! {
                status = output => status,
                never = admitting => match never {},
            };
            if native {
                if let Some(mut state) = workspace.read_job_state(workflow).await? {
                    for result in state.jobs.values_mut().chain(
                        state
                            .variants
                            .values_mut()
                            .flat_map(|variants| variants.values_mut()),
                    ) {
                        result
                            .outputs
                            .retain(|_, value| !masker.contains(value));
                    }
                    tokio::select! {
                        result = deliver(|| service.queue.job_state(worker, run.id, &generation, workflow, &state)) => result?,
                        _ = process::cancelled(cancel.clone()) => return Err(Stopped::Cancelled.into()),
                        _ = tokio::time::sleep(deadline.saturating_duration_since(Instant::now())) => return Err(Stopped::TimedOut.into()),
                    }
                }
            }
            // The batches end once the last sender is gone, which is when the output ends.
            drop(lines_tx);
            status
        };
        let sending = async {
            tokio::select! {
                sent = send_log_batches(service, worker, run, lines_rx, &mut log_open) => sent,
                _ = process::cancelled(cancel.clone()) => Ok(()),
                _ = tokio::time::sleep(deadline.saturating_duration_since(Instant::now())) => Ok(()),
            }
        };
        let (status, sent) = tokio::join!(running, sending);
        sent?;
        let status = status?;
        if native && status.code() == Some(codeyard_actions::runner::CANCELLED_EXIT_CODE) {
            return Err(Stopped::Cancelled.into());
        }
        if !status.success() {
            return Ok(false);
        }
    }
    Ok(true)
}

const ADMISSION_POLL: Duration = Duration::from_secs(1);

/// Answers the runner's job concurrency requests until the runner exits. A waiting job learns
/// when it holds its group, and a running one when a newer job in the group cancels it. The
/// answers are files in a private directory the runner polls.
async fn answer_admissions(
    service: &CiWorkers,
    worker: &str,
    run: i64,
    generation: &str,
    workflow: &str,
    jobs: &Mutex<BTreeMap<(String, u32), i64>>,
    directory: &Path,
) -> std::convert::Infallible {
    let mut answered = BTreeMap::<i64, bool>::new();
    loop {
        tokio::time::sleep(ADMISSION_POLL).await;
        let waiting: Vec<_> = jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|(_, id)| answered.get(*id) != Some(&false))
            .map(|(key, id)| (key.clone(), *id))
            .collect();
        for ((job, variant), id) in waiting {
            let answer = match service
                .queue
                .admit_job(worker, run, generation, workflow, id)
                .await
            {
                Ok(Some(answer)) => answer,
                Ok(None) => continue,
                Err(error) => {
                    tracing::debug!(%error, "job concurrency check failed");
                    continue;
                }
            };
            if answered.get(&id) == Some(&answer) {
                continue;
            }
            match write_admission(directory, &job, variant, answer) {
                Ok(()) => {
                    answered.insert(id, answer);
                }
                Err(error) => tracing::warn!(%error, "could not answer a job concurrency request"),
            }
        }
    }
}

/// Replaces the answer atomically, so the runner never reads half a file.
fn write_admission(directory: &Path, job: &str, variant: u32, granted: bool) -> Result<()> {
    ensure!(
        !job.is_empty() && !job.starts_with('.') && !job.contains(['/', '\\']),
        "invalid job id"
    );
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    let answer = if granted { "granted" } else { "cancelled" };
    std::io::Write::write_all(file.as_file_mut(), answer.as_bytes())?;
    file.persist(directory.join(format!("{job}.{variant}")))?;
    Ok(())
}

/// The ref GitHub reports for a run. The checkout is detached and a database-free worker has
/// no branches in it, so act cannot find the ref itself and would guess `refs/heads/master`.
fn github_ref(claim: &ClaimedRun) -> Option<String> {
    let run = &claim.run;
    if run.event == "pull_request" {
        return claim
            .pull_request
            .as_ref()
            .map(|pull| format!("refs/pull/{}/merge", pull.number));
    }
    Some(
        if run.branch.starts_with("refs/") && run.event != "pull_request_target" {
            run.branch.clone()
        } else {
            format!("refs/heads/{}", run.branch)
        },
    )
}

fn act_command(
    service: &CiWorkers,
    claim: &ClaimedRun,
    workspace: &Workspace,
    definition: &Path,
    labels: &BTreeSet<String>,
    secret_file: Option<&Path>,
    podman_label: Option<&str>,
) -> tokio::process::Command {
    let config = &service.config;
    let run = &claim.run;
    let native = run.engine.as_deref() == Some("codeyard-actions");
    let home = workspace.root().join("home");
    let mut command = process::command(if native {
        &config.actions_binary
    } else {
        &config.act_binary
    });
    #[cfg(target_os = "linux")]
    {
        let parent = std::process::id();
        // A dead server must not leave act submitting new containers during recovery. The
        // native runner gets SIGTERM instead, so its cancellation stops the job's commands.
        let death = if native { libc::SIGTERM } else { libc::SIGKILL };
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, death) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() as u32 != parent {
                    return Err(std::io::Error::other("CI worker exited before act started"));
                }
                Ok(())
            });
        }
    }
    command
        .arg(&run.event)
        .arg("-W")
        .arg(definition)
        .arg("--directory")
        .arg(workspace.checkout())
        // act reads `.actrc` from its working directory; keep the repository's out of it.
        .current_dir(workspace.root())
        .env("HOME", &home)
        .env("TMPDIR", workspace.root().join("tmp"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("DOCKER_CONFIG", home.join(".docker"))
        .env("GITSERV_RUN_ID", run.run_id.to_string())
        .env("GITSERV_PIPELINE", &run.pipeline)
        .env("GITSERV_REPO_OWNER", &claim.owner)
        .env("GITSERV_REPO_NAME", &claim.repository_name)
        .env("GITSERV_BRANCH", &run.branch)
        .env("GITSERV_HEAD_SHA", run.head_sha.as_deref().unwrap_or(""));
    if !native {
        // act names a job's container, network and volumes after the workflow and job alone, so
        // two runs of one workflow would fight over them. The patched act adds this value to those
        // names and leaves the workflow's own name, which `github.workflow` shows, unchanged.
        command.env("CODEYARD_ACT_INSTANCE", run.run_id.simple().to_string());
    }
    if !claim.runtime.token.is_empty() {
        // act derives RESULTS_URL from its parent's RUNTIME_URL after loading --env.
        command
            .env("ACTIONS_RUNTIME_URL", &claim.runtime.results_url)
            .env("ACTIONS_RUNTIME_TOKEN", &claim.runtime.token);
    }
    // Workflow URLs come from the core, so a runner without PUBLIC_URL still names the forge.
    let origin = if claim.forge_origin.is_empty() {
        crate::backend::api::http::config::link_origin()
    } else {
        claim.forge_origin.clone()
    };
    for (name, value) in [
        ("GITHUB_SERVER_URL", origin.clone()),
        ("GITHUB_API_URL", format!("{origin}/api/v3")),
        ("GITHUB_GRAPHQL_URL", format!("{origin}/api/graphql")),
        ("GITHUB_SHA", run.head_sha.clone().unwrap_or_default()),
        ("SHA_REF", run.head_sha.clone().unwrap_or_default()),
        ("GITHUB_WORKFLOW_SHA", run.workflow_sha.as_ref().or(run.head_sha.as_ref()).cloned().unwrap_or_default()),
        ("GITHUB_WORKFLOW_REF", {
            let workflow = run.workflow_path.split(',').find(|path| workspace.definition(path) == definition).unwrap_or(&run.workflow_path);
            format!("{}/{}/{workflow}@{}", claim.owner, claim.repository_name, github_ref(claim).unwrap_or_default())
        }),
        (
            "GITHUB_RUN_ID",
            run.workflow_run_id.unwrap_or(run.id).to_string(),
        ),
        (
            "GITHUB_RUN_ATTEMPT",
            run.workflow_run_attempt.unwrap_or(1).to_string(),
        ),
        (
            "GITHUB_RUN_NUMBER",
            run.workflow_run_number.unwrap_or(run.id).to_string(),
        ),
    ] {
        command.arg("--env").arg(format!("{name}={value}"));
    }
    if let Some(environment) = &run.environment {
        command.env("GITSERV_ENVIRONMENT", environment);
    }
    // What `github.repository`, `github.actor` and `github.ref` show in the workflow.
    command.arg("--env").arg(format!(
        "GITHUB_REPOSITORY={}/{}",
        claim.owner, claim.repository_name
    ));
    let actor = claim
        .original_actor
        .as_ref()
        .map(|actor| actor.login.as_str())
        .unwrap_or(&claim.actor);
    if !actor.is_empty() {
        command.arg("--actor").arg(actor);
    }
    if !claim.actor.is_empty() {
        command
            .arg("--env")
            .arg(format!("GITHUB_TRIGGERING_ACTOR={}", claim.actor));
    }
    if let Some(actor) = &claim.original_actor {
        command
            .arg("--env")
            .arg(format!("GITHUB_ACTOR_ID={}", actor.id));
    }
    if claim.debug {
        command.arg(if native { "--debug" } else { "--verbose" });
        for (key, value) in [
            ("ACTIONS_RUNNER_DEBUG", "true"),
            ("ACTIONS_STEP_DEBUG", "true"),
            ("RUNNER_DEBUG", "1"),
        ] {
            command.arg("--env").arg(format!("{key}={value}"));
        }
    }
    if claim.run.event_payload.is_some()
        || claim.pull_request.is_some()
        || claim.run.event == "workflow_dispatch"
    {
        command.arg("--eventpath").arg(workspace.event_path());
    }
    if let Some(reference) = github_ref(claim) {
        command.arg("--env").arg(format!("GITHUB_REF={reference}"));
    }
    if native && config.github_action_fallback { command.arg("--github-action-fallback"); }
    match config.mode {
        CiExecutionMode::Local => {
            command.env("CODEYARD_DENY_DOCKER", "1");
            if native { command.arg("--deny-docker"); }
            for label in labels {
                command.arg("-P").arg(format!("{label}=-self-hosted"));
            }
        }
        CiExecutionMode::Podman => {
            for (label, image) in &config.act_platform_map {
                // The site's runner image replaces the node's for the Ubuntu labels.
                let image = match &claim.limits.image {
                    Some(site) if super::config::UBUNTU_LABELS.contains(&label.as_str()) => site,
                    _ => image,
                };
                command.arg("-P").arg(format!("{label}={image}"));
            }
            let socket = podman_socket_path(&config.podman_socket)
                .expect("CiConfig contains a validated Podman socket");
            command.env("DOCKER_HOST", format!("unix://{socket}"));
            // Use the runner image that is already on the node; act pulls it only when it is
            // missing. Pulling on every run costs a registry round trip and fails offline.
            command.arg("--pull=false");
            // One job at a time, so the limits below bound the whole run.
            command.arg("--concurrent-jobs").arg("1");
            if let Some(label) = podman_label {
                // Explicit job/service containers also receive these caps during rewriting.
                let limits = claim.limits.container_options();
                let options = run_container_options(label);
                command
                    .arg("--container-options")
                    .arg(if limits.is_empty() {
                        options
                    } else {
                        format!("{limits} {options}")
                    });
            }
        }
    }
    // Never mount the container engine socket into job containers.
    command.arg("--container-daemon-socket").arg("-");
    if let Some(file) = secret_file {
        command.arg("--secret-file").arg(file);
    }
    command
}

fn run_container_label(run_id: uuid::Uuid) -> String {
    format!("{RUN_LABEL_KEY}={run_id}")
}

/// The label that finds a run's containers, and a one-second stop grace: a job container's
/// first process ignores SIGTERM, so the default makes every run wait ten seconds to clean up.
fn run_container_options(label: &str) -> String {
    format!("--label {label} --stop-timeout 1")
}

fn container_filters(label: &str) -> String {
    serde_json::to_string(&BTreeMap::from([("label", [label])]))
        .expect("the static container filter is serializable")
}

#[cfg(test)]
fn prepare_workflow(
    path: &Path,
    config: &super::config::CiConfig,
    run_id: uuid::Uuid,
) -> Result<BTreeSet<String>> {
    prepare_workflow_for_engine(path, config, run_id, false, "")
}

fn prepare_workflow_for_engine(
    path: &Path,
    config: &super::config::CiConfig,
    run_id: uuid::Uuid,
    native: bool,
    limits: &str,
) -> Result<BTreeSet<String>> {
    let mut value = super::workflows::parse_yaml(&std::fs::read(path)?)?;
    ensure!(native || !codeyard_actions::workflow::has_job_concurrency(&value), "Job concurrency requires the codeyard-actions engine");
    if config.mode != CiExecutionMode::Local {
        for job in value.get_mut("jobs").and_then(Value::as_mapping_mut).into_iter().flat_map(|jobs| jobs.values_mut()) {
            if let Some(selector) = job.get_mut("runs-on").and_then(Value::as_mapping_mut) {
                if selector.contains_key("group") && selector.get("labels").is_none_or(|labels| labels.as_sequence().is_some_and(Vec::is_empty)) {
                    let label = config.act_platform_map.keys().find(|label| label.as_str() == "ubuntu-latest").or_else(|| config.act_platform_map.keys().next()).context("Group-only jobs need a configured default platform")?;
                    selector.insert("labels".into(), label.clone().into());
                }
            }
        }
    }
    let labels = labels_from_value(&value, config.mode == CiExecutionMode::Local)?;
    if config.mode == CiExecutionMode::Local {
        return Ok(labels);
    }
    for job in value.get("jobs").and_then(Value::as_mapping).into_iter().flat_map(|jobs| jobs.values()) {
        if job.get("uses").is_some() { continue; }
        let selector = job.get("runs-on").context("Job needs runs-on")?;
        let selector = selector.get("labels").unwrap_or(selector);
        let selected: Vec<_> = match selector {
            Value::String(label) => vec![label.as_str()],
            Value::Sequence(labels) => labels.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        ensure!(selected.iter().any(|label| config.act_platform_map.keys().any(|key| key.eq_ignore_ascii_case(label))), "no runner image matches this job's runs-on selectors; add one to CI_ACT_PLATFORM_MAP");
    }
    add_container_options(&mut value, &format!("{} {limits}", run_container_options(&run_container_label(run_id))))?;
    std::fs::write(path, serde_yaml::to_string(&value)?)?;
    Ok(labels)
}

fn add_container_options(workflow: &mut Value, option: &str) -> Result<()> {
    let Some(jobs) = workflow.get_mut("jobs").and_then(Value::as_mapping_mut) else {
        bail!("workflow jobs must be a mapping");
    };
    for job in jobs.values_mut() {
        let Some(job) = job.as_mapping_mut() else {
            continue;
        };
        if let Some(container) = job.get_mut(Value::String("container".into())) {
            append_container_option(container, option)?;
        }
        if let Some(services) = job
            .get_mut(Value::String("services".into()))
            .and_then(Value::as_mapping_mut)
        {
            // The parser accepts the image shorthand for services too; an empty image there
            // disables the service, and the runner leaves it out.
            for service in services.values_mut() {
                append_container_option(service, option)?;
            }
        }
    }
    Ok(())
}

fn append_container_option(spec: &mut Value, option: &str) -> Result<()> {
    if let Value::String(image) = spec {
        let mut expanded = Mapping::new();
        expanded.insert(Value::String("image".into()), Value::String(image.clone()));
        expanded.insert(
            Value::String("options".into()),
            Value::String(option.to_owned()),
        );
        *spec = Value::Mapping(expanded);
        return Ok(());
    }
    let Some(mapping) = spec.as_mapping_mut() else {
        bail!("workflow container configuration must be a mapping");
    };
    let key = Value::String("options".into());
    match mapping.get_mut(&key) {
        None | Some(Value::Null) => {
            mapping.insert(key, Value::String(option.to_owned()));
        }
        Some(Value::String(options)) => {
            if !options.is_empty() {
                options.push(' ');
            }
            options.push_str(option);
        }
        Some(_) => bail!("workflow container options must be a string"),
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct ContainerRecord {
    socket: String,
    run_id: uuid::Uuid,
    #[serde(default)]
    cgroup: Option<PathBuf>,
}

struct ContainerOwnership {
    // An exclusive file lock is the local execution claim, including while the core is offline.
    _lock: std::fs::File,
    path: PathBuf,
    record: ContainerRecord,
    group: Option<codeyard_actions::process::Cgroup>,
}

impl Drop for ContainerOwnership {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // A concurrent fork can hold a duplicate until exec. Release our finished lock
        // explicitly rather than waiting for every inherited descriptor to close.
        unsafe {
            libc::flock(self._lock.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl ContainerOwnership {
    fn create(directory: &Path, socket: &str, run_id: uuid::Uuid, cgroup_root: Option<&Path>) -> Result<Self> {
        let record = ContainerRecord {
            socket: socket.to_owned(),
            run_id,
            cgroup: cgroup_root.map(|root| root.join(format!("codeyard-{run_id}"))),
        };
        let mut pending = tempfile::NamedTempFile::new_in(directory)?;
        ensure!(
            lock_container_record(pending.as_file())?,
            "CI cleanup record is busy"
        );
        serde_json::to_writer(&mut pending, &record)?;
        pending.as_file().sync_all()?;
        let path = directory.join(format!("{run_id}.json"));
        let file = pending.persist_noclobber(&path)?;
        std::fs::File::open(directory)?.sync_all()?;
        // Persist recovery identity before creating anything in the delegated hierarchy.
        let group = cgroup_root.map(|root| codeyard_actions::process::Cgroup::create(root, &format!("codeyard-{run_id}"))).transpose()?;
        Ok(Self {
            _lock: file,
            path,
            record,
            group,
        })
    }

    async fn remove(&self) -> Result<()> {
        self.remove_containers().await?;
        self.remove_workspace().await
    }

    async fn remove_containers(&self) -> Result<()> {
        self.fence().await?;
        if let Some(group) = &self.group { group.cleanup(Duration::from_secs(30)).await?; }
        Ok(())
    }

    async fn fence(&self) -> Result<()> {
        if let Some(group) = &self.group { group.kill()?; }
        if !self.record.socket.is_empty() {
            remove_run_containers(
                &self.record.socket,
                &run_container_label(self.record.run_id),
            )
            .await?;
        }
        Ok(())
    }

    async fn remove_workspace(&self) -> Result<()> {
        let directory = self.path.with_extension("workspace");
        let mut locks = Vec::new();
        match tokio::fs::read_dir(&directory).await {
            Ok(mut entries) => while let Some(entry) = entries.next_entry().await? {
                if entry.file_type().await?.is_dir() {
                    locks.push(crate::backend::git::locks::lock(&entry.path().join("repo")).await);
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        match tokio::fs::remove_dir_all(&directory).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        std::fs::remove_file(&self.path)?;
        std::fs::File::open(self.path.parent().unwrap())?.sync_all()?;
        Ok(())
    }
}

fn lock_container_record(file: &std::fs::File) -> Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: the fd is owned and flock does not retain pointers.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error.into())
    }
}

async fn prepare_slot(directory: &Path) -> Result<()> {
    // Check that new ownership records can be persisted before accepting another run.
    use std::io::Write;
    let mut probe = tempfile::NamedTempFile::new_in(directory)?;
    probe.write_all(b"CI cleanup record")?;
    probe.as_file().sync_all()?;
    reconcile_containers(directory).await
}

pub(super) async fn reconcile_containers(directory: &Path) -> Result<()> {
    let mut result = Ok(());
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let cleaned = async {
            let file = match std::fs::File::open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            if !lock_container_record(&file)? {
                return Ok(());
            }
            // A concurrent reconciler may have unlinked this inode before releasing its lock.
            if !path.exists() {
                return Ok(());
            }
            let record: ContainerRecord = serde_json::from_reader(&file)?;
            let group = match record.cgroup.as_deref() {
                Some(path) => match std::fs::symlink_metadata(path) {
                    Ok(_) => Some(codeyard_actions::process::Cgroup::open(path)?),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error.into()),
                },
                None => None,
            };
            ContainerOwnership {
                _lock: file,
                path,
                record,
                group,
            }
            .remove()
            .await
        }
        .await;
        if let Err(error) = cleaned {
            warn!(%error, "could not clean an abandoned CI run");
            result = Err(error);
        }
    }
    result
}

pub(super) async fn recover_containers(directory: PathBuf, mut stop: watch::Receiver<bool>) {
    while !*stop.borrow() {
        if let Err(error) = reconcile_containers(&directory).await {
            warn!(%error, "could not recover CI containers");
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            _ = stop.changed() => {}
        }
    }
}

#[derive(Deserialize)]
struct ResourceId {
    #[serde(rename = "Id")]
    id: String,
}

#[derive(Deserialize)]
struct NamedResource {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name", default)]
    name: String,
}

#[derive(Deserialize)]
struct ContainerDetail {
    #[serde(rename = "Name", default)]
    name: Option<String>,
    #[serde(rename = "Mounts", default)]
    mounts: Option<Vec<ContainerMount>>,
}

#[derive(Deserialize)]
struct ContainerMount {
    #[serde(rename = "Type", default)]
    kind: Option<String>,
    #[serde(rename = "Name", default)]
    name: Option<String>,
}

/// act names a job container's workspace and environment volumes after the container, and its
/// service network after the container and job, but gives none of them the run label. They are
/// found through the labeled job container, so no other volume or network is touched.
fn act_owned_names(detail: &ContainerDetail) -> Option<(String, Vec<String>)> {
    let container = detail.name.as_deref()?.trim_start_matches('/');
    let valid = |name: &str| {
        !name.is_empty()
            && name.len() <= 255
            && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    if !container.starts_with("act-") || !valid(container) {
        return None;
    }
    let environment = format!("{container}-env");
    let volumes = detail
        .mounts
        .iter()
        .flatten()
        .filter(|mount| mount.kind.as_deref() == Some("volume"))
        .filter_map(|mount| mount.name.as_deref())
        .filter(|name| *name == container || *name == environment)
        .map(str::to_owned)
        .collect();
    Some((container.to_owned(), volumes))
}

/// Force-removes every container carrying the run's label through the Podman socket, then the
/// act volumes and networks those containers owned.
async fn remove_run_containers(socket: &str, label: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .unix_socket(podman_socket_path(socket)?)
        .timeout(CLEANUP_TIMEOUT)
        .build()?;
    let filters = container_filters(label);
    let containers: Vec<ResourceId> = client
        .get("http://localhost/v1.40/containers/json")
        .query(&[("all", "true"), ("filters", filters.as_str())])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut job_containers = Vec::new();
    for container in containers {
        // Read the names first: they are gone with the container.
        let response = client
            .get(format!("http://localhost/v1.40/containers/{}/json", container.id))
            .send()
            .await?;
        let owned = if response.status() == reqwest::StatusCode::NOT_FOUND {
            None
        } else {
            let detail: ContainerDetail = response.error_for_status()?.json().await?;
            act_owned_names(&detail)
        };
        // Anonymous volumes, which images such as postgres declare, go with the container.
        remove_resource(
            &client,
            &format!("containers/{}", container.id),
            &[("force", "true"), ("v", "true")],
        )
        .await?;
        if let Some((name, volumes)) = owned {
            for volume in volumes {
                remove_unlabeled_resource(&client, &format!("volumes/{volume}")).await;
            }
            job_containers.push(name);
        }
    }
    if !job_containers.is_empty() {
        let networks: Vec<NamedResource> = client
            .get("http://localhost/v1.40/networks")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        for network in networks.into_iter().filter(|network| {
            network.name.ends_with("-network")
                && job_containers
                    .iter()
                    .any(|container| network.name.starts_with(&format!("{container}-")))
        }) {
            remove_unlabeled_resource(&client, &format!("networks/{}", network.id)).await;
        }
    }
    // Networks and built images carry the run's label as well, so a crash leaves none behind.
    let networks: Vec<ResourceId> = client
        .get("http://localhost/v1.40/networks")
        .query(&[("filters", filters.as_str())])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for network in networks {
        remove_resource(&client, &format!("networks/{}", network.id), &[]).await?;
    }
    let images: Vec<ResourceId> = client
        .get("http://localhost/v1.40/images/json")
        .query(&[("filters", filters.as_str())])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for image in images {
        remove_resource(
            &client,
            &format!("images/{}", image.id),
            &[("force", "true")],
        )
        .await?;
    }
    Ok(())
}

/// Deletes an act volume or network that has no run label. Another unlabeled container may still
/// use it, so a refusal is logged and does not keep the run's cleanup record.
async fn remove_unlabeled_resource(client: &reqwest::Client, path: &str) {
    if let Err(error) = remove_resource(client, path, &[]).await {
        warn!(%error, resource = path, "could not remove an act resource of a CI run");
    }
}

/// Deletes one Podman resource. A resource that is already gone is not an error.
async fn remove_resource(
    client: &reqwest::Client,
    path: &str,
    query: &[(&str, &str)],
) -> Result<()> {
    let response = client
        .delete(format!("http://localhost/v1.40/{path}"))
        .query(query)
        .send()
        .await?;
    if response.status() != reqwest::StatusCode::NOT_FOUND {
        response.error_for_status()?;
    }
    Ok(())
}

/// Preserve ordinary delivery, but spend at most one short wait on a stalled remote queue.
async fn enqueue_log_line(
    sender: &mpsc::Sender<LogLine>,
    line: LogLine,
    remote: bool,
    stalled: &AtomicBool,
) -> Result<()> {
    if !remote {
        return sender
            .send(line)
            .await
            .map_err(|_| anyhow::anyhow!("CI log batches stopped"));
    }
    if stalled.load(Ordering::Relaxed) {
        let _ = sender.try_send(line);
    } else if tokio::time::timeout(LOG_FLUSH_GRACE, sender.send(line))
        .await
        .is_err()
    {
        stalled.store(true, Ordering::Relaxed);
    }
    Ok(())
}

/// Stores output lines. The Actions pages read the stored lines.
async fn send_log_lines(
    service: &CiWorkers,
    worker: &str,
    run: &CiRun,
    lines: &[LogLine],
) -> Result<bool> {
    let result = service.queue.logs(worker, run.id, lines).await;
    // Log ingestion is best effort on both database and remote workers.
    match result {
        Err(error) => {
            warn!(%error, "could not send CI log lines");
            Ok(true)
        }
        result => result,
    }
}

/// Stores the lines a command prints, in order, in batches. A batch goes out when it is full,
/// or when `LOG_BATCH_INTERVAL` has passed while lines wait. What is left goes out when the
/// output ends.
async fn send_log_batches(
    service: &CiWorkers,
    worker: &str,
    run: &CiRun,
    mut lines: mpsc::Receiver<LogLine>,
    log_open: &mut bool,
) -> Result<()> {
    let mut batch: Vec<LogLine> = Vec::new();
    let mut bytes = 0;
    let mut tick = tokio::time::interval(LOG_BATCH_INTERVAL);
    // After a quiet spell the next batch is not flushed once per missed tick.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { break };
                if !*log_open {
                    continue;
                }
                bytes += line.stage.len() + line.line.len();
                batch.push(line);
                if batch.len() < LOG_BATCH_LINES && bytes < LOG_BATCH_BYTES {
                    continue;
                }
            }
            _ = tick.tick(), if !batch.is_empty() => {}
        }
        *log_open = send_log_lines(service, worker, run, &batch).await?;
        batch.clear();
        bytes = 0;
    }
    if batch.is_empty() {
        return Ok(());
    }
    *log_open = send_log_lines(service, worker, run, &batch).await?;
    Ok(())
}

fn credential_masker(claim: &ClaimedRun) -> codeyard_actions::runner::Masker {
    let masker = codeyard_actions::runner::Masker::default();
    let values = std::iter::once(&claim.runtime.token)
        .chain(std::iter::once(&claim.github_tokens.act))
        .chain(std::iter::once(&claim.github_tokens.completion_secret))
        .chain(
            claim
                .github_tokens
                .workflows
                .values()
                .flat_map(|jobs| jobs.values()),
        )
        .chain(claim.secrets.iter().map(|(_, value)| value))
        .chain(
            claim
                .environment_values
                .environments
                .values()
                .flat_map(|scope| scope.secrets.values()),
        )
        .filter(|value| !value.is_empty());
    for value in values {
        masker.add(value);
    }
    masker
}

fn log_level(line: &str) -> &'static str {
    let lower = line.to_ascii_lowercase();
    if lower.contains("error") {
        "error"
    } else if lower.contains("warn") {
        "warn"
    } else {
        "info"
    }
}

/// Temporary directory with the checkout, the exact workflow files and a home
/// directory for act. Dropping it transfers removal to the worker's blocking cleanup.
struct Workspace {
    temp: Option<tempfile::TempDir>,
    cleanups: Arc<Mutex<JoinSet<()>>>,
    cleaned: Option<oneshot::Sender<()>>,
    removable: bool,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if !self.removable {
            if let Some(temp) = self.temp.take() { let _ = temp.keep(); }
            return;
        }
        let checkout = self.checkout();
        let temp = self.temp.take().unwrap();
        let cleaned = self.cleaned.take();
        let mut cleanups = self.cleanups.lock().unwrap_or_else(|e| e.into_inner());
        while cleanups.try_join_next().is_some() {}
        cleanups.spawn(async move {
            // A cancelled checkout may still be importing its pack on a blocking thread.
            let _lock = crate::backend::git::locks::lock(&checkout).await;
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(error) = temp.close() {
                    warn!(%error, "could not remove CI workspace");
                }
                if let Some(cleaned) = cleaned {
                    let _ = cleaned.send(());
                }
            })
            .await;
        });
    }
}

impl Workspace {
    fn restore_called_workflows(&self, run: &CiRun) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let blobs = run.workflow_blobs.as_object().context("run has no workflow files")?;
        let called: Vec<_> = blobs.keys().filter(|path| !run.workflow_path.split(',').any(|root| root == path.as_str())).collect();
        if called.is_empty() { return Ok(()); }
        let checkout = self.checkout();
        for path in [".github", ".github/workflows", ".git", ".git/info"] {
            let directory = checkout.join(path);
            match std::fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure!(std::fs::symlink_metadata(&directory)?.is_dir(), "Unsafe workflow directory");
                }
                Err(error) => return Err(error.into()),
            }
        }
        let mut exclude = std::fs::OpenOptions::new().append(true).create(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(checkout.join(".git/info/exclude"))?;
        ensure!(exclude.metadata()?.is_file(), "Unsafe Git exclude file");
        for workflow in called {
            ensure!(workflow.starts_with(".github/workflows/codeyard-call-") && super::admission::is_safe_workflow_path(workflow), "Invalid called workflow path");
            let target = checkout.join(workflow);
            match std::fs::remove_file(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(target)?;
            file.write_all(&std::fs::read(self.definition(workflow))?)?;
            writeln!(exclude, "\n/{workflow}")?;
        }
        Ok(())
    }

    async fn prepare(
        queue: &RunQueue,
        claim: &ClaimedRun,
        directory: &Path,
        cleanups: Arc<Mutex<JoinSet<()>>>,
        cleaned: oneshot::Sender<()>,
    ) -> Result<Self> {
        tokio::fs::create_dir_all(directory).await?;
        let workspace = Self {
            temp: Some(codeyard_actions::fs::private_tempdir("workspace-", Some(directory)).context("could not create a CI workspace")?),
            cleanups,
            cleaned: Some(cleaned),
            removable: false,
        };
        for dir in ["home/.docker", "definitions", "tmp"] {
            tokio::fs::create_dir_all(workspace.root().join(dir)).await?;
        }
        let store = queue.checkout(claim, &workspace.checkout()).await?;
        let run = &claim.run;
        // Run exactly the workflow files that were checked when the run was queued.
        let blobs = run
            .workflow_blobs
            .as_object()
            .context("the run has no workflow files")?;
        let mut definitions = BTreeMap::new();
        for (path, id) in blobs {
            let content = if let Some(content) = super::reusable::content(id) {
                ensure!(
                    super::admission::is_safe_workflow_path(path),
                    "invalid workflow path"
                );
                content.as_bytes().to_vec()
            } else {
                store
                    .blob_by_id(id.as_str().context("bad workflow blob id")?)
                    .await?
            };
            definitions.insert(path.clone(), content);
        }
        for (path, content) in definitions {
            tokio::fs::write(workspace.definition(&path), content).await?;
        }
        Ok(workspace)
    }

    fn root(&self) -> &Path {
        self.temp.as_ref().unwrap().path()
    }

    fn checkout(&self) -> PathBuf {
        self.root().join("repo")
    }

    fn definition(&self, workflow: &str) -> PathBuf {
        self.root().join("definitions").join(file_name(workflow))
    }

    /// The private directory where the worker answers a workflow's job concurrency requests.
    fn concurrency_dir(&self, workflow: &str) -> Result<PathBuf> {
        let path = self
            .root()
            .join(format!("{}.concurrency", file_name(workflow)));
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn job_state_path(&self, workflow: &str) -> PathBuf {
        self.root()
            .join(format!("{}.state.json", file_name(workflow)))
    }

    async fn read_job_state(
        &self,
        workflow: &str,
    ) -> Result<Option<codeyard_actions::progress::State>> {
        use tokio::io::AsyncReadExt;
        let file = match tokio::fs::File::open(self.job_state_path(workflow)).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut body = Vec::new();
        file.take((codeyard_actions::progress::MAX_STATE_BYTES + 1) as u64)
            .read_to_end(&mut body)
            .await?;
        ensure!(
            body.len() <= codeyard_actions::progress::MAX_STATE_BYTES,
            "job state exceeds the native limit"
        );
        Ok(Some(
            serde_json::from_slice(&body).context("invalid private job state")?,
        ))
    }

    fn event_path(&self) -> PathBuf {
        self.root().join("event.json")
    }

    /// Old runs retain their compact pull request context; new runs use the webhook document.
    fn write_event(&self, claim: &ClaimedRun) -> Result<()> {
        let event = if let Some(payload) = &claim.run.event_payload {
            payload.clone()
        } else if let Some(pull) = &claim.pull_request {
            let head_sha = claim
                .run
                .head_sha
                .as_deref()
                .context("the run has no commit")?;
            serde_json::json!({
                "action": "synchronize",
                "number": pull.number,
                "pull_request": {
                    "number": pull.number,
                    "title": pull.title,
                    "head": { "ref": pull.head_ref, "sha": head_sha },
                    "base": { "ref": pull.base_ref, "sha": pull.base_sha },
                },
            })
        } else if claim.run.event == "workflow_dispatch" {
            // act types `inputs.*` from the declared input types, as GitHub does.
            let inputs = claim
                .run
                .workflow_inputs
                .clone()
                .unwrap_or_else(|| serde_json::json!({}));
            serde_json::json!({ "inputs": inputs })
        } else {
            return Ok(());
        };
        std::fs::write(self.event_path(), serde_json::to_vec(&event)?)?;
        Ok(())
    }

    fn write_github_secrets(
        &self,
        secrets: &[(String, String)],
        token: &str,
    ) -> Result<Option<PathBuf>> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.write_secrets(secrets)?;
        if token.is_empty() {
            return Ok(path);
        }
        let path = path.unwrap_or_else(|| self.root().join("secrets.env"));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        writeln!(file, "GITHUB_TOKEN={}", dotenv_value(token))?;
        Ok(Some(path))
    }

    fn write_environment_values(
        &self,
        workflow: &str,
        values: &super::environment_values::EnvironmentValues,
        fork: bool,
    ) -> Result<Option<PathBuf>> {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let Some(jobs) = values
            .workflows
            .get(workflow)
            .filter(|jobs| !jobs.is_empty())
        else {
            return Ok(None);
        };
        let mut environments = BTreeMap::new();
        for key in jobs.values().collect::<BTreeSet<_>>() {
            let scope = values
                .environments
                .get(key)
                .context("Invalid environment value binding")?;
            ensure!(
                !fork || scope.secrets.is_empty(),
                "Fork claims cannot contain environment secrets"
            );
            let secrets: Vec<_> = scope
                .secrets
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            db::secrets::validate_repository_secrets(&secrets)?;
            ensure!(
                scope.variables.len() <= codeyard_actions::values::MAX_SCOPE_VALUES,
                "Too many environment variables"
            );
            let variables: Vec<_> = scope
                .variables
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            db::variables::validate_all(&variables)?;
            environments.insert(key, scope);
        }
        let bytes =
            serde_json::to_vec(&serde_json::json!({"jobs":jobs,"environments":environments}))?;
        ensure!(
            bytes.len() <= codeyard_actions::values::MAX_ENVIRONMENT_FILE_BYTES,
            "Environment values file is too large"
        );
        let path = self
            .root()
            .join(format!("{}.environments.json", file_name(workflow)));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(&bytes)?;
        Ok(Some(path))
    }

    fn write_replay(
        &self,
        workflow: &str,
        replay: &codeyard_actions::progress::Replay,
    ) -> Result<PathBuf> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = self
            .root()
            .join(format!(".replay-{}.json", file_name(workflow)));
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        serde_json::to_writer(file, replay)?;
        Ok(path)
    }

    fn write_job_tokens(
        &self,
        workflow: &str,
        tokens: &BTreeMap<String, String>,
        credentials: &super::tokens::GithubTokens,
    ) -> Result<PathBuf> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = self
            .root()
            .join(format!("{}.tokens.json", file_name(workflow)));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        serde_json::to_writer(
            file,
            &serde_json::json!({"tokens": tokens, "completion_secret": credentials.completion_secret,
                "oidc_url": credentials.oidc_url,
                "oidc_jobs": tokens.keys().filter(|job| credentials.oidc_jobs.contains(&format!("{workflow}:{job}"))).collect::<Vec<_>>()
            }),
        )?;
        Ok(path)
    }

    /// act's dotenv parser needs different escaping from the in-house runner's parser.
    fn write_secrets(&self, secrets: &[(String, String)]) -> Result<Option<PathBuf>> {
        self.write_secrets_for(secrets, true)
    }

    fn write_secrets_for(
        &self,
        secrets: &[(String, String)],
        act: bool,
    ) -> Result<Option<PathBuf>> {
        if secrets.is_empty() {
            return Ok(None);
        }
        // Remote claims are untrusted input here. Validate the whole set before the file exists.
        db::secrets::validate_workflow_secrets(secrets)?;

        self.write_values("secrets.env", secrets, act)
    }

    fn write_values(
        &self,
        filename: &str,
        values: &[(String, String)],
        act: bool,
    ) -> Result<Option<PathBuf>> {
        if values.is_empty() {
            return Ok(None);
        }
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.root().join(filename);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        for (name, value) in values {
            writeln!(
                file,
                "{name}={}",
                if act {
                    dotenv_value(value)
                } else {
                    native_dotenv_value(value)
                }
            )?;
        }
        Ok(Some(path))
    }
}

fn native_dotenv_value(value: &str) -> String {
    let mut encoded = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => encoded.push_str("\\\\"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push('\r'),
            '"' => encoded.push_str("\\\""),
            '$' => encoded.push_str("\\$"),
            _ => encoded.push(character),
        }
    }
    encoded.push('"');
    encoded
}

fn dotenv_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() + 8);
    encoded.push('"');
    for character in value.chars() {
        match character {
            '\\' => encoded.push_str("\\\\"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '"' => encoded.push_str("\\\""),
            '!' => encoded.push_str("\\!"),
            '$' => encoded.push_str("\\$"),
            '`' => encoded.push_str("\\`"),
            _ => encoded.push(character),
        }
    }
    // act 0.2.89 pins godotenv 1.5.1, whose quote scanner mishandles escaped
    // endings. Its extra quote is trimmed when a backslash ends the value. For
    // other endings, `0` cannot be a validated name, so the expansion is empty.
    if value.ends_with('\\') {
        encoded.push_str("\"\"");
    } else {
        encoded.push_str("${0}\"");
    }
    encoded
}

fn file_name(workflow: &str) -> &str {
    workflow.rsplit('/').next().unwrap_or("workflow.yml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ci::CiConfig, db::DbPool, repositories::store::RepoStore};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn preparation_validates_and_rewrites_one_workflow_value() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ci.yml");
        let source = b"name: Test\njobs: {build: {runs-on: ubuntu-latest, container: alpine}}\n";
        std::fs::write(&path, source).unwrap();
        let config = super::super::config::CiConfig {
            mode: CiExecutionMode::Podman,
            ..Default::default()
        };
        let run_id = uuid::Uuid::new_v4();
        let labels = prepare_workflow(&path, &config, run_id).unwrap();
        assert!(labels.contains("ubuntu-latest"));
        let value = super::super::workflows::parse_yaml(&std::fs::read(&path).unwrap()).unwrap();
        // The workflow keeps its public name; act's resource names carry the run instead.
        assert_eq!(value["name"].as_str(), Some("Test"));
        assert!(value["jobs"]["build"]["container"]["options"]
            .as_str()
            .unwrap()
            .contains(&run_container_label(run_id)));
        std::fs::write(&path, source).unwrap();
        prepare_workflow_for_engine(&path, &config, run_id, true, "").unwrap();
        let native = super::super::workflows::parse_yaml(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(native["name"].as_str(), Some("Test"));
        assert!(native["jobs"]["build"]["container"]["options"]
            .as_str()
            .unwrap()
            .contains(&run_container_label(run_id)));
        std::fs::write(&path, source).unwrap();
        let config = super::super::config::CiConfig {
            mode: CiExecutionMode::Local,
            ..Default::default()
        };
        assert!(prepare_workflow(&path, &config, run_id).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), source);
        let local = b"jobs: {build: {runs-on: linux, steps: [{run: echo ok}]}}";
        std::fs::write(&path, local).unwrap();
        prepare_workflow(&path, &config, run_id).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), local);
    }

    async fn queued_fixture(root: &Path) -> (CiWorkers, CiRun) {
        let pool = db::init_db(&format!("sqlite://{}", root.join("test.db").display()))
            .await
            .unwrap();
        db::run_migrations(&pool).await.unwrap();
        let owner = db::users::accounts::create_user(&pool, "worker_owner", None)
            .await
            .unwrap();
        let repo = db::repositories::records::create_repository(
            &pool,
            owner.id,
            "project",
            None,
            true,
            "/fixture.git",
        )
        .await
        .unwrap();
        let sha = "1".repeat(40);
        let run = db::ci::enqueue_ci_run(
            &pool,
            db::ci::NewCiRun {
                repo_id: repo.id,
                triggered_by_id: owner.id,
                pipeline: "ci",
                event: "push",
                pull_request_id: None,
                branch: "main",
                head_sha: &sha,
                workflow_path: "ci.yml",
                workflow_blobs: &serde_json::json!({"ci.yml": sha}),
                workflow_inputs: None,
                environment: None,
                manual: false,
            },
            &[owner.id],
        )
        .await
        .unwrap();
        db::ci::claim_ci_run(&pool, "worker", "local", &[owner.id])
            .await
            .unwrap()
            .unwrap();
        let config = Arc::new(super::super::config::CiConfig::default());
        let service = CiWorkers::new(
            RunQueue::Database {
                pool: pool.clone(),
                config: config.clone(),
                secrets: None,
                context: None,
                notifications: Arc::new(
                    crate::backend::notifications::NotificationBroadcaster::new(&pool),
                ),
            },
            config,
            false,
            root,
        );
        std::fs::create_dir(&service.cleanup_dir).unwrap();
        (service, run)
    }

    #[tokio::test]
    async fn closed_logs_drain_output_without_sending_more_batches() {
        let root = tempfile::tempdir().unwrap();
        let (service, run) = queued_fixture(root.path()).await;
        let RunQueue::Database { pool, .. } = &service.queue else {
            unreachable!()
        };
        let DbPool::Sqlite(raw) = pool else {
            unreachable!()
        };
        sqlx::query("UPDATE ci_runs SET log_lines = ? WHERE id = ?")
            .bind(super::super::logs::MAX_RUN_LOG_LINES - 2)
            .bind(run.id)
            .execute(raw)
            .await
            .unwrap();
        let (tx, rx) = mpsc::channel(256);
        let producing = async {
            for _ in 0..64 {
                tx.send(LogLine {
                    stage: "ci".into(),
                    line: "output".into(),
                    level: "info".into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            }
            loop {
                let stored: i64 = sqlx::query_scalar("SELECT log_lines FROM ci_runs WHERE id = ?")
                    .bind(run.id)
                    .fetch_one(raw)
                    .await
                    .unwrap();
                if stored == super::super::logs::MAX_RUN_LOG_LINES {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            db::ci::finish_ci_run(pool, run.id, "worker", "success", None)
                .await
                .unwrap()
                .unwrap();
            // Any later ingestion would now fail ownership validation.
            for _ in 0..128 {
                tx.send(LogLine {
                    stage: "ci".into(),
                    line: "discarded".into(),
                    level: "info".into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            }
            drop(tx);
        };
        let mut open = true;
        let (_, sent) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                producing,
                send_log_batches(&service, "worker", &run, rx, &mut open)
            )
        })
        .await
        .unwrap();
        sent.unwrap();
        assert!(!open);
        assert_eq!(
            db::ci::list_ci_logs(pool, run.id, 100).await.unwrap().len(),
            2
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn unavailable_cleanup_finishes_the_run_but_quarantines_the_slot() {
        let root = tempfile::tempdir().unwrap();
        let (mut service, run) = queued_fixture(root.path()).await;
        Arc::make_mut(&mut service.config).podman_socket =
            root.path().join("missing.sock").to_str().unwrap().into();
        Arc::make_mut(&mut service.config).mode = CiExecutionMode::Podman;
        let claim = ClaimedRun {
            run: run.clone(),
            storage_node_id: None,
            repository: uuid::Uuid::new_v4(),
            repository_name: "project".into(),
            owner: "worker_owner".into(),
            actor: "worker_owner".into(),
            original_actor: None,
            forge_origin: String::new(),
            replay: None,
            debug: false,
            secrets: vec![],
            variables: Vec::new(),
            environment_values: Default::default(),
            github_tokens: Default::default(),
            pull_request: None,
            storage_peer: None,
            storage_replicas: Vec::new(),
            limits: Default::default(),
            runtime: Default::default(),
            store: None,
        };
        let (stop, receiver) = watch::channel(false);
        let task_service = service.clone();
        let running =
            tokio::spawn(async move { execute(&task_service, "worker", claim, receiver).await });
        let RunQueue::Database { pool, .. } = &service.queue else {
            unreachable!()
        };
        let current = tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let current = db::ci::get_ci_run_by_run_id(pool, run.run_id)
                    .await
                    .unwrap()
                    .unwrap();
                if current.completed_at.is_some() {
                    break current;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(current.status, "failed");
        assert!(current.error_message.unwrap().contains("cleanup failed"));
        assert!(!running.is_finished());
        assert!(service
            .cleanup_dir
            .join(format!("{}.json", run.run_id))
            .exists());
        stop.send(true).unwrap();
        assert!(!tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .unwrap()
            .unwrap());
        assert!(reconcile_containers(&service.cleanup_dir).await.is_err());
        pool.close().await;
    }

    fn secret_workspace() -> Workspace {
        Workspace {
            temp: Some(tempfile::tempdir().unwrap()),
            cleanups: Arc::new(Mutex::new(JoinSet::new())),
            cleaned: None,
            removable: true,
        }
    }

    #[tokio::test]
    async fn workspace_cleanup_waits_for_an_in_progress_pack_import() {
        let mut workspace = secret_workspace();
        let root = workspace.root().to_owned();
        let lock = crate::backend::git::locks::lock(&workspace.checkout()).await;
        let (cleaned, mut cleanup) = oneshot::channel();
        workspace.cleaned = Some(cleaned);
        let cleanups = workspace.cleanups.clone();
        drop(workspace);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut cleanup)
                .await
                .is_err()
        );
        assert!(root.exists());
        drop(lock);
        tokio::time::timeout(Duration::from_secs(5), cleanup)
            .await
            .unwrap()
            .unwrap();
        assert!(!root.exists());
        // Retain and drain the same owner used by the worker.
        let mut tasks = std::mem::take(&mut *cleanups.lock().unwrap());
        while tasks.join_next().await.is_some() {}
    }

    #[tokio::test]
    async fn environment_values_files_are_private_and_bound_to_one_workflow() {
        let workspace = secret_workspace();
        let mut values = super::super::environment_values::EnvironmentValues::default();
        for (key, secret) in [("prod", "production-secret"), ("other", "other-secret")] {
            values.environments.insert(
                key.into(),
                db::environment_values::WorkflowValues {
                    secrets: BTreeMap::from([("TOKEN".into(), secret.into())]),
                    variables: BTreeMap::from([("COLOR".into(), "visible-variable".into())]),
                },
            );
        }
        values.workflows.insert(
            "ci.yml".into(),
            BTreeMap::from([("deploy".into(), "prod".into())]),
        );
        values.workflows.insert(
            "other.yml".into(),
            BTreeMap::from([("deploy".into(), "other".into())]),
        );
        let path = workspace
            .write_environment_values("ci.yml", &values, false)
            .unwrap()
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(body["jobs"]["deploy"], "prod");
        assert_eq!(body["environments"].as_object().unwrap().len(), 1);
        assert_eq!(
            body["environments"]["prod"]["variables"]["COLOR"],
            "visible-variable"
        );
        assert!(!path.starts_with(workspace.checkout()));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let fork = secret_workspace();
        assert!(fork
            .write_environment_values("ci.yml", &values, true)
            .is_err());
        assert_eq!(std::fs::read_dir(fork.root()).unwrap().count(), 0);
        assert!(workspace
            .write_environment_values("missing.yml", &values, false)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn environment_values_masking_retains_shadowed_secrets_but_not_variables() {
        let root = tempfile::tempdir().unwrap();
        let (_, run) = queued_fixture(root.path()).await;
        let mut environment_values = super::super::environment_values::EnvironmentValues::default();
        environment_values.environments.insert(
            "prod".into(),
            db::environment_values::WorkflowValues {
                secrets: BTreeMap::from([("TOKEN".into(), "shared-secret".into())]),
                variables: BTreeMap::from([("COLOR".into(), "visible-variable".into())]),
            },
        );
        let claim = ClaimedRun {
            run,
            storage_node_id: None,
            repository: uuid::Uuid::new_v4(),
            repository_name: "project".into(),
            owner: "worker_owner".into(),
            actor: "worker_owner".into(),
            original_actor: None,
            forge_origin: String::new(),
            replay: None,
            debug: false,
            secrets: vec![("TOKEN".into(), "shared".into())],
            variables: Vec::new(),
            environment_values,
            github_tokens: Default::default(),
            pull_request: None,
            storage_peer: None,
            storage_replicas: Vec::new(),
            limits: Default::default(),
            runtime: Default::default(),
            store: None,
        };
        assert_eq!(
            credential_masker(&claim).mask("shared-secret shared visible-variable"),
            "*** *** visible-variable"
        );
    }

    fn complex_secret_value() -> String {
        " private marker \\path\nline\rreturn\"quote!bang$dollar`tick` # ' = tail\\".to_owned()
    }

    #[tokio::test]
    async fn secret_file_is_validated_encoded_and_private() {
        let workspace = secret_workspace();
        let complex = complex_secret_value();
        let path = workspace
            .write_secrets(&[
                ("COMPLEX".into(), complex.clone()),
                ("TRAILING_QUOTE".into(), "ends\"".into()),
                ("EMPTY".into(), String::new()),
            ])
            .unwrap()
            .unwrap();
        assert_eq!(
            dotenv_value(&complex),
            "\" private marker \\\\path\\nline\\rreturn\\\"quote\\!bang\\$dollar\\`tick\\` # ' = tail\\\\\"\""
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            concat!(
                "COMPLEX=\" private marker \\\\path\\nline\\rreturn\\\"quote\\!bang\\$dollar\\`tick\\` # ' = tail\\\\\"\"\n",
                "TRAILING_QUOTE=\"ends\\\"${0}\"\n",
                "EMPTY=\"${0}\"\n",
            )
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn invalid_decrypted_pair_never_creates_a_secret_file_or_discloses_its_value() {
        let marker = "private-value-must-not-appear";
        for (name, value, message) in [
            (
                "github_token",
                marker.to_owned(),
                "Repository secret name uses a reserved prefix",
            ),
            (
                "VALID_NAME",
                format!("{marker}\0tail"),
                "Repository secret value contains NUL",
            ),
            (
                "VALID_NAME",
                "x".repeat(db::secrets::MAX_SECRET_VALUE_BYTES + 1),
                "Repository secret value exceeds 48 KiB",
            ),
        ] {
            let workspace = secret_workspace();
            let error = workspace
                .write_secrets(&[("VALID_FIRST".into(), "safe".into()), (name.into(), value)])
                .unwrap_err();
            assert_eq!(error.to_string(), message);
            assert!(!error.to_string().contains(marker));
            assert!(!workspace.root().join("secrets.env").exists());
        }
    }

    #[tokio::test]
    #[ignore = "requires the pinned act 0.2.89 binary (GITSERV_TEST_ACT_BINARY)"]
    async fn pinned_act_reads_complex_dotenv_values_losslessly() {
        use sha2::{Digest, Sha256};

        let act = std::env::var("GITSERV_TEST_ACT_BINARY")
            .expect("GITSERV_TEST_ACT_BINARY must name the pinned act binary");
        assert_eq!(
            hex::encode(Sha256::digest(std::fs::read(&act).unwrap())),
            "6be37b104430efc210d5130495bedcff2dc7cd6780a38d88f3d205e7f1185cc1"
        );

        let workspace = secret_workspace();
        let complex = complex_secret_value();
        let trailing_quote = "ends\"".to_owned();
        let secret_file = workspace
            .write_secrets(&[
                ("COMPLEX".into(), complex.clone()),
                ("TRAILING_QUOTE".into(), trailing_quote.clone()),
            ])
            .unwrap()
            .unwrap();
        let repository_path = workspace.root().join("repo");
        std::fs::create_dir_all(&repository_path).unwrap();
        let repository = git2::Repository::init(&repository_path).unwrap();
        std::fs::write(repository_path.join("README.md"), "fixture\n").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();

        let workflow = workspace.root().join("secret-transport.yml");
        let complex_ok = workspace.root().join("complex-value-ok");
        let trailing_quote_ok = workspace.root().join("trailing-quote-value-ok");
        std::fs::write(
            &workflow,
            format!(
                "name: secret transport\non: [push]\njobs:\n  check:\n    runs-on: gitserv-secret-test\n    steps:\n      - shell: bash\n        env:\n          COMPLEX: ${{{{ secrets.COMPLEX }}}}\n          TRAILING_QUOTE: ${{{{ secrets.TRAILING_QUOTE }}}}\n        run: |\n          test \"$(printf '%s' \"$COMPLEX\" | sha256sum | cut -d' ' -f1)\" = \"{}\"\n          touch \"{}\"\n          test \"$(printf '%s' \"$TRAILING_QUOTE\" | sha256sum | cut -d' ' -f1)\" = \"{}\"\n          touch \"{}\"\n",
                hex::encode(Sha256::digest(complex.as_bytes())),
                complex_ok.display(),
                hex::encode(Sha256::digest(trailing_quote.as_bytes())),
                trailing_quote_ok.display(),
            ),
        )
        .unwrap();
        let home = workspace.root().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut command = process::command(act);
        command
            .arg("push")
            .arg("-W")
            .arg(&workflow)
            .arg("--directory")
            .arg(&repository_path)
            .arg("-P")
            .arg("gitserv-secret-test=-self-hosted")
            .arg("--secret-file")
            .arg(&secret_file)
            .arg("--container-daemon-socket")
            .arg("-")
            .arg("--no-cache-server")
            .arg("--action-offline-mode")
            .current_dir(workspace.root())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_CACHE_HOME", home.join("cache"));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let captured = lines.clone();
        let (_stop_tx, stop) = watch::channel(false);
        let status = process::run(command, Duration::from_secs(30), stop, move |line, _| {
            captured.lock().unwrap().push(line);
            async { Ok(()) }
        })
        .await
        .unwrap();
        let lines = lines.lock().unwrap();
        if !status.success() {
            panic!(
                "pinned act failed (complex={}, trailing_quote={})",
                complex_ok.exists(),
                trailing_quote_ok.exists()
            );
        }
        assert!(!lines.iter().any(|line| line.contains("private marker")));
        assert!(!lines.iter().any(|line| line.contains("ends\"")));
    }

    struct FakeResponse {
        status: u16,
        body: Vec<u8>,
    }

    async fn fake_podman(
        responses: Vec<FakeResponse>,
    ) -> (
        tempfile::TempDir,
        String,
        mpsc::UnboundedReceiver<(String, String)>,
        tokio::task::JoinHandle<()>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("podman.sock");
        let socket_text = socket.to_string_lossy().into_owned();
        let (requests_tx, requests_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let listener = match tokio::net::UnixListener::bind(&socket) {
                Ok(listener) => {
                    let _ = ready_tx.send(Ok::<_, std::io::Error>(()));
                    listener
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut bytes = [0; 1024];
                    let read = stream.read(&mut bytes).await.unwrap();
                    assert!(read > 0, "request ended before its headers");
                    request.extend_from_slice(&bytes[..read]);
                    assert!(request.len() <= 16 * 1024, "request headers were unbounded");
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(request).unwrap();
                let mut first = request.lines().next().unwrap().split_whitespace();
                let method = first.next().unwrap().to_owned();
                let target = first.next().unwrap().to_owned();
                requests_tx.send((method, target)).unwrap();
                let reason = match response.status {
                    200 => "OK",
                    204 => "No Content",
                    404 => "Not Found",
                    500 => "Internal Server Error",
                    503 => "Service Unavailable",
                    status => panic!("unsupported fake status {status}"),
                };
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            response.status,
                            reason,
                            response.body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                stream.write_all(&response.body).await.unwrap();
            }
        });
        ready_rx.await.unwrap().unwrap();
        (directory, socket_text, requests_rx, task)
    }

    fn response(status: u16, body: impl Into<Vec<u8>>) -> FakeResponse {
        FakeResponse {
            status,
            body: body.into(),
        }
    }

    fn list_body(ids: &[&str]) -> Vec<u8> {
        serde_json::to_vec(
            &ids.iter()
                .map(|id| serde_json::json!({ "Id": id }))
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    fn decoded_query(target: &str) -> BTreeMap<String, String> {
        let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
        url.query_pairs().into_owned().collect()
    }

    #[tokio::test]
    async fn results_and_oidc_credentials_reach_act_and_its_job_environment() {
        let root = tempfile::tempdir().unwrap();
        let (service, mut run) = queued_fixture(root.path()).await;
        run.engine = Some("act".into());
        let mut claim = ClaimedRun {
            run,
            storage_node_id: None,
            repository: uuid::Uuid::new_v4(),
            repository_name: "project".into(),
            owner: "worker_owner".into(),
            actor: "worker_owner".into(),
            original_actor: None,
            forge_origin: String::new(),
            replay: None,
            debug: false,
            secrets: Vec::new(),
            variables: Vec::new(),
            environment_values: Default::default(),
            pull_request: None,
            storage_peer: None,
            storage_replicas: Vec::new(),
            limits: Default::default(),
            github_tokens: Default::default(),
            runtime: super::super::runtime::RuntimeCredentials {
                token: "runtime-test-token".into(),
                results_url: "https://results.example".into(),
            },
            store: None,
        };
        claim.github_tokens.act = "oidc-test-token".into();
        claim.github_tokens.oidc_url = "https://issuer.example/api/ci/oidc/token?".into();
        claim.github_tokens.oidc_jobs.insert("act".into());
        let command = act_command(
            &service,
            &claim,
            &secret_workspace(),
            Path::new("workflow.yml"),
            &BTreeSet::new(),
            None,
            None,
        );
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for value in [
            "ACTIONS_ID_TOKEN_REQUEST_TOKEN=oidc-test-token",
            "ACTIONS_ID_TOKEN_REQUEST_URL=https://issuer.example/api/ci/oidc/token?",
            "ACTIONS_RUNTIME_TOKEN=runtime-test-token",
            "ACTIONS_RESULTS_URL=https://results.example",
            "ACTIONS_CACHE_SERVICE_V2=True",
        ] {
            assert!(args.windows(2).any(|pair| pair == ["--env", value]));
        }
        let env = command
            .as_std()
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(env["ACTIONS_RUNTIME_TOKEN"], "runtime-test-token");
        assert_eq!(env["ACTIONS_RUNTIME_URL"], "https://results.example");
        claim.github_tokens.oidc_jobs.clear();
        let command = act_command(
            &service,
            &claim,
            &secret_workspace(),
            Path::new("workflow.yml"),
            &BTreeSet::new(),
            None,
            None,
        );
        assert!(!command.as_std().get_args().any(|arg| arg
            .to_string_lossy()
            .starts_with("ACTIONS_ID_TOKEN_REQUEST_")));
    }

    #[test]
    fn run_labels_filters_and_workflow_options_are_exact() {
        let run_id = uuid::Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let label = run_container_label(run_id);
        assert_eq!(
            label,
            "com.codeyard.ci.run=01234567-89ab-cdef-0123-456789abcdef"
        );
        assert_eq!(
            container_filters(&label),
            r#"{"label":["com.codeyard.ci.run=01234567-89ab-cdef-0123-456789abcdef"]}"#
        );

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("workflow.yml");
        std::fs::write(
            &path,
            r#"on: push
jobs:
  host:
    runs-on: ubuntu-latest
  image-shorthand:
    runs-on: ubuntu-latest
    container: alpine:latest
  explicit:
    runs-on: ubuntu-latest
    container:
      image: alpine:latest
      options: --read-only
    services:
      database:
        image: postgres:17
        options: --tmpfs /tmp
      cache:
        image: redis:7
"#,
        )
        .unwrap();
        let mut yaml = super::super::workflows::parse_yaml(&std::fs::read(path).unwrap()).unwrap();
        add_container_options(&mut yaml, &run_container_options(&label)).unwrap();
        let jobs = yaml.get("jobs").unwrap();
        assert_eq!(
            jobs["image-shorthand"]["container"]["options"],
            run_container_options(&label)
        );
        assert_eq!(
            jobs["explicit"]["container"]["options"],
            format!("--read-only {}", run_container_options(&label))
        );
        assert_eq!(
            jobs["explicit"]["services"]["database"]["options"],
            format!("--tmpfs /tmp {}", run_container_options(&label))
        );
        assert_eq!(
            jobs["explicit"]["services"]["cache"]["options"],
            run_container_options(&label)
        );
    }

    #[tokio::test]
    async fn recovery_keeps_live_claims_and_retries_abandoned_ownership() {
        const CHILD: &str = "GITSERV_CI_RECOVERY_CHILD";
        if let Ok(input) = std::env::var(CHILD) {
            let (directory, socket, run): (PathBuf, String, uuid::Uuid) =
                serde_json::from_str(&input).unwrap();
            let _ownership = ContainerOwnership::create(&directory, &socket, run, None).unwrap();
            let workspace = directory.join(format!("{run}.workspace"));
            std::fs::create_dir(&workspace).unwrap();
            std::fs::write(workspace.join("secrets.env"), "TOKEN=private").unwrap();
            unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
            unreachable!("SIGKILL does not return");
        }
        let records = tempfile::tempdir().unwrap();
        let run = uuid::Uuid::new_v4();
        let (_directory, socket, mut requests, server) = fake_podman(vec![
            response(503, Vec::new()),
            response(200, list_body(&["workflow", "service"])),
            response(204, Vec::new()),
            response(204, Vec::new()),
            // Networks, then built images, carry the same run label.
            response(200, list_body(&[])),
            response(200, list_body(&[])),
        ])
        .await;
        let live = uuid::Uuid::new_v4();
        let _ownership = ContainerOwnership::create(records.path(), &socket, live, None).unwrap();
        let live_workspace = records.path().join(format!("{live}.workspace"));
        std::fs::create_dir(&live_workspace).unwrap();
        reconcile_containers(records.path()).await.unwrap();
        assert!(live_workspace.exists());
        assert!(
            requests.try_recv().is_err(),
            "a live worker must not be cleaned"
        );
        use std::os::unix::process::ExitStatusExt;
        let killed = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("recovery_keeps_live_claims_and_retries_abandoned_ownership")
            .env(
                CHILD,
                serde_json::to_string(&(records.path(), &socket, run)).unwrap(),
            )
            .output()
            .unwrap();
        assert_eq!(killed.status.signal(), Some(libc::SIGKILL));
        let path = records.path().join(format!("{run}.json"));
        let stored: ContainerRecord =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored.run_id, run);
        assert_eq!(stored.socket, socket);
        let workspace = path.with_extension("workspace");
        assert!(workspace.join("secrets.env").exists());
        assert!(reconcile_containers(records.path()).await.is_err());
        assert!(workspace.exists());
        assert!(
            path.exists(),
            "engine failures must keep durable cleanup work"
        );
        // A process forked by a parallel test can briefly hold a copy of the lock the
        // previous pass had open. That pass then looks like a live worker and sends no
        // request, exactly as the periodic recovery retries it 30 seconds later.
        for _ in 0..50 {
            reconcile_containers(records.path()).await.unwrap();
            if !path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!path.exists());
        assert!(!workspace.exists());
        assert!(live_workspace.exists());
        assert!(records.path().join(format!("{live}.json")).exists());
        server.await.unwrap();
        for _ in 0..2 {
            let (method, target) = requests.recv().await.unwrap();
            assert_eq!(method, "GET");
            assert_eq!(
                decoded_query(&target)["filters"],
                container_filters(&run_container_label(run))
            );
        }
    }

    #[tokio::test]
    async fn local_run_recovery_removes_abandoned_workspaces() {
        let records = tempfile::tempdir().unwrap();
        let run = uuid::Uuid::new_v4();
        let ownership = ContainerOwnership::create(records.path(), "", run, None).unwrap();
        let workspace = ownership.path.with_extension("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("secrets.env"), "TOKEN=private").unwrap();
        reconcile_containers(records.path()).await.unwrap();
        assert!(workspace.exists());
        let inherited = ownership._lock.try_clone().unwrap();
        drop(ownership);
        reconcile_containers(records.path()).await.unwrap();
        assert!(!workspace.exists());
        drop(inherited);
    }

    #[tokio::test]
    async fn stalled_remote_log_buffer_drops_overflow_without_repeated_waits() {
        let (sender, mut receiver) = mpsc::channel(1);
        let line = || LogLine {
            stage: "ci".into(),
            line: "output".into(),
            level: "info".into(),
            ..Default::default()
        };
        let stalled = AtomicBool::new(false);
        enqueue_log_line(&sender, line(), true, &stalled)
            .await
            .unwrap();
        enqueue_log_line(&sender, line(), true, &stalled)
            .await
            .unwrap();
        assert!(stalled.load(Ordering::Relaxed));
        tokio::time::timeout(Duration::from_millis(100), async {
            for _ in 0..1000 {
                enqueue_log_line(&sender, line(), true, &stalled)
                    .await
                    .unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(receiver.recv().await.unwrap().line, "output");
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn cleanup_removes_only_containers_with_the_run_label() {
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        let label = "com.codeyard.ci.run=01234567-89ab-cdef-0123-456789abcdef";
        let network = "c".repeat(64);
        let act_network = "d".repeat(64);
        let inspected = serde_json::to_vec(&serde_json::json!({
            "Name": "/act-job",
            "Mounts": [
                { "Type": "volume", "Name": "act-job" },
                { "Type": "volume", "Name": "act-job-env" },
                { "Type": "volume", "Name": "user-data" },
                { "Type": "bind", "Name": "act-job" },
            ],
        }))
        .unwrap();
        let networks = serde_json::to_vec(&serde_json::json!([
            { "Id": act_network, "Name": "act-job-build-network" },
            { "Id": "user", "Name": "user-network" },
        ]))
        .unwrap();
        let (_directory, socket, mut requests, server) = fake_podman(vec![
            response(200, list_body(&[&first, &second])),
            response(200, inspected),
            response(204, Vec::new()),
            response(204, Vec::new()),
            response(204, Vec::new()),
            response(404, Vec::new()),
            response(404, Vec::new()),
            response(200, networks),
            response(204, Vec::new()),
            response(200, list_body(&[&network])),
            response(204, Vec::new()),
            response(200, list_body(&[])),
        ])
        .await;

        remove_run_containers(&format!("unix://{socket}"), label)
            .await
            .unwrap();
        server.await.unwrap();

        let (method, target) = requests.recv().await.unwrap();
        assert_eq!(method, "GET");
        assert_eq!(
            decoded_query(&target).get("filters").map(String::as_str),
            Some(container_filters(label).as_str())
        );
        let (method, target) = requests.recv().await.unwrap();
        assert_eq!((method.as_str(), target.as_str()), ("GET", format!("/v1.40/containers/{first}/json").as_str()));
        let (method, target) = requests.recv().await.unwrap();
        assert_eq!(method, "DELETE");
        assert!(target.starts_with(&format!("/v1.40/containers/{first}?")));
        let query = decoded_query(&target);
        assert_eq!(query.get("force").map(String::as_str), Some("true"));
        assert_eq!(query.get("v").map(String::as_str), Some("true"));
        for volume in ["act-job", "act-job-env"] {
            let (method, target) = requests.recv().await.unwrap();
            assert_eq!(method, "DELETE");
            assert!(target.starts_with(&format!("/v1.40/volumes/{volume}")));
        }
        let (method, target) = requests.recv().await.unwrap();
        assert_eq!((method.as_str(), target.as_str()), ("GET", format!("/v1.40/containers/{second}/json").as_str()));
        let (method, target) = requests.recv().await.unwrap();
        assert_eq!(method, "DELETE");
        assert!(target.starts_with(&format!("/v1.40/containers/{second}?")));
        let (method, target) = requests.recv().await.unwrap();
        assert_eq!((method.as_str(), target.as_str()), ("GET", "/v1.40/networks"));
        let (method, target) = requests.recv().await.unwrap();
        assert_eq!(method, "DELETE");
        assert!(target.starts_with(&format!("/v1.40/networks/{act_network}")));
        let (method, target) = requests.recv().await.unwrap();
        assert_eq!(method, "GET");
        assert!(target.starts_with("/v1.40/networks?"));
        assert_eq!(
            decoded_query(&target).get("filters").map(String::as_str),
            Some(container_filters(label).as_str())
        );
        let (method, target) = requests.recv().await.unwrap();
        assert_eq!(method, "DELETE");
        assert!(target.starts_with(&format!("/v1.40/networks/{network}")));
        let (method, target) = requests.recv().await.unwrap();
        assert_eq!(method, "GET");
        assert!(target.starts_with("/v1.40/images/json?"));

        let (_directory, socket, _requests, server) =
            fake_podman(vec![response(503, Vec::new())]).await;
        assert!(remove_run_containers(&socket, label).await.is_err());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn workspace_holds_the_runs_commit_and_the_workflow_files_queued_with_it() {
        const CHILD: &str = "GITSERV_CI_WORKSPACE_UMASK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::process::CommandExt;
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .arg("workspace_holds_the_runs_commit_and_the_workflow_files_queued_with_it")
                .arg("--nocapture")
                .env(CHILD, "1");
            unsafe {
                command.pre_exec(|| {
                    libc::umask(0o022);
                    Ok(())
                });
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let root = tempfile::tempdir().unwrap();
        let store = RepoStore::create(root.path(), "main", None).await.unwrap();
        let repo = git2::Repository::open_bare(store.location()).unwrap();
        let workflow = repo.blob(b"on: push\n").unwrap();
        let mut workflows = repo.treebuilder(None).unwrap();
        workflows.insert("ci.yml", workflow, 0o100644).unwrap();
        let mut github = repo.treebuilder(None).unwrap();
        github
            .insert("workflows", workflows.write().unwrap(), 0o040000)
            .unwrap();
        let mut top = repo.treebuilder(None).unwrap();
        top.insert(".github", github.write().unwrap(), 0o040000)
            .unwrap();
        top.insert("README.md", repo.blob(b"# hi\n").unwrap(), 0o100644)
            .unwrap();
        let tree = repo.find_tree(top.write().unwrap()).unwrap();
        let author = git2::Signature::now("Tester", "tester@example.invalid").unwrap();
        let commit = repo
            .commit(Some("refs/heads/main"), &author, &author, "ci", &tree, &[])
            .unwrap();
        let run: CiRun = serde_json::from_value(serde_json::json!({
            "id": 1,
            "run_id": "00000000-0000-0000-0000-000000000001",
            "repo_id": 1,
            "triggered_by_id": null,
            "mode": "local",
            "pipeline": "ci",
            "event": "push",
            "branch": "main",
            "head_sha": commit.to_string(),
            "workflow_path": ".github/workflows/ci.yml",
            "workflow_sha": null,
            "workflow_blobs": { ".github/workflows/ci.yml": workflow.to_string() },
            "worker": null,
            "heartbeat_at": null,
            "cancel_requested": false,
            "environment": null,
            "status": "running",
            "error_message": null,
            "started_at": null,
            "completed_at": null,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap();

        let cleanups = Arc::new(Mutex::new(JoinSet::new()));
        let (cleaned, cleanup) = oneshot::channel();
        let workspace_parent = tempfile::tempdir().unwrap();
        let pool = crate::backend::db::init_db("sqlite::memory:")
            .await
            .unwrap();
        let queue = RunQueue::Database {
            pool: pool.clone(),
            config: Arc::new(CiConfig::default()),
            secrets: None,
            context: None,
            notifications: Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                &pool,
            )),
        };
        let claim = ClaimedRun {
            run,
            storage_node_id: None,
            repository: uuid::Uuid::new_v4(),
            repository_name: "test".into(),
            owner: "test".into(),
            actor: "test".into(),
            original_actor: None,
            forge_origin: String::new(),
            replay: None,
            debug: false,
            secrets: Vec::new(),
            variables: Vec::new(),
            environment_values: Default::default(),
            pull_request: None,
            storage_peer: None,
            storage_replicas: Vec::new(),
            limits: Default::default(),
            runtime: Default::default(),
            github_tokens: Default::default(),
            store: Some(store),
        };
        let workspace = Workspace::prepare(
            &queue,
            &claim,
            workspace_parent.path(),
            cleanups.clone(),
            cleaned,
        )
        .await
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(workspace.root())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        assert_eq!(
            std::fs::read_to_string(workspace.checkout().join("README.md")).unwrap(),
            "# hi\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.definition(".github/workflows/ci.yml")).unwrap(),
            "on: push\n"
        );
        let path = workspace.root().to_owned();
        drop(workspace);
        cleanup.await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_manual_run_gets_its_inputs_as_github_event_inputs() {
        let run: CiRun = serde_json::from_value(serde_json::json!({
            "id": 1,
            "run_id": "00000000-0000-0000-0000-000000000001",
            "repo_id": 1,
            "triggered_by_id": null,
            "mode": "local",
            "pipeline": "ci",
            "event": "workflow_dispatch",
            "branch": "main",
            "head_sha": "a".repeat(40),
            "workflow_path": ".github/workflows/deploy.yml",
            "workflow_sha": null,
            "workflow_blobs": {},
            "workflow_inputs": { "target": "staging", "debug": "true" },
            "worker": null,
            "heartbeat_at": null,
            "cancel_requested": false,
            "environment": null,
            "status": "running",
            "error_message": null,
            "started_at": null,
            "completed_at": null,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap();
        let claim = ClaimedRun {
            run,
            storage_node_id: None,
            repository: uuid::Uuid::nil(),
            repository_name: "repo".to_owned(),
            owner: "alice".to_owned(),
            actor: String::new(),
            original_actor: None,
            forge_origin: String::new(),
            replay: None,
            debug: false,
            secrets: Vec::new(),
            variables: Vec::new(),
            environment_values: Default::default(),
            github_tokens: Default::default(),
            pull_request: None,
            storage_peer: None,
            storage_replicas: Vec::new(),
            limits: Default::default(),
            runtime: Default::default(),
            store: None,
        };
        let workspace = secret_workspace();
        workspace.write_event(&claim).unwrap();
        let event: serde_json::Value =
            serde_json::from_slice(&std::fs::read(workspace.event_path()).unwrap()).unwrap();
        assert_eq!(event["inputs"]["target"], "staging");
        assert_eq!(event["inputs"]["debug"], "true");
    }

    #[tokio::test]
    async fn a_pull_request_run_gets_its_event_and_ref() {
        let head = "a".repeat(40);
        let run: CiRun = serde_json::from_value(serde_json::json!({
            "id": 1,
            "run_id": "00000000-0000-0000-0000-000000000001",
            "repo_id": 1,
            "triggered_by_id": null,
            "mode": "local",
            "pipeline": "ci",
            "event": "pull_request",
            "pull_request_id": 9,
            "branch": "feature",
            "head_sha": head,
            "workflow_path": ".github/workflows/ci.yml",
            "workflow_sha": null,
            "workflow_blobs": {},
            "worker": null,
            "heartbeat_at": null,
            "cancel_requested": false,
            "environment": null,
            "status": "running",
            "error_message": null,
            "started_at": null,
            "completed_at": null,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap();
        let claim = ClaimedRun {
            run,
            storage_node_id: None,
            repository: uuid::Uuid::nil(),
            repository_name: "repo".to_owned(),
            owner: "alice".to_owned(),
            actor: String::new(),
            original_actor: None,
            forge_origin: String::new(),
            replay: None,
            debug: false,
            secrets: Vec::new(),
            variables: Vec::new(),
            environment_values: Default::default(),
            github_tokens: Default::default(),
            pull_request: Some(crate::backend::ci::queue::PullRequestEvent {
                number: 7,
                title: "Add a feature".to_owned(),
                head_ref: "feature".to_owned(),
                base_ref: "main".to_owned(),
                base_sha: "b".repeat(40),
            }),
            storage_peer: None,
            storage_replicas: Vec::new(),
            limits: Default::default(),
            runtime: Default::default(),
            store: None,
        };
        let workspace = secret_workspace();
        workspace.write_event(&claim).unwrap();
        let event: serde_json::Value =
            serde_json::from_slice(&std::fs::read(workspace.event_path()).unwrap()).unwrap();
        assert_eq!(event["number"], 7);
        assert_eq!(event["pull_request"]["title"], "Add a feature");
        assert_eq!(event["pull_request"]["head"]["ref"], "feature");
        assert_eq!(event["pull_request"]["head"]["sha"], head);
        assert_eq!(event["pull_request"]["base"]["ref"], "main");
        assert_eq!(github_ref(&claim).as_deref(), Some("refs/pull/7/merge"));
        // pull_request_target stores the base branch's short name, even one that looks like a ref.
        let mut target = claim;
        target.run.event = "pull_request_target".to_owned();
        target.run.branch = "refs/heads/main".to_owned();
        assert_eq!(
            github_ref(&target).as_deref(),
            Some("refs/heads/refs/heads/main")
        );
    }

    #[tokio::test]
    async fn an_unrecorded_result_is_retried_until_the_worker_stops() {
        let native = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let pool = DbPool::from(native);
        db::run_migrations(&pool).await.unwrap();
        let owner = db::users::accounts::create_user(&pool, "result_owner", None)
            .await
            .unwrap();
        let repository = db::repositories::records::create_repository(
            &pool,
            owner.id,
            "result",
            None,
            true,
            "/unused/result.git",
        )
        .await
        .unwrap();
        let sha = "a".repeat(40);
        let blobs = serde_json::json!({ ".github/workflows/ci.yml": sha });
        let run = db::ci::enqueue_ci_run(
            &pool,
            db::ci::NewCiRun {
                repo_id: repository.id,
                triggered_by_id: owner.id,
                pipeline: "ci",
                event: "push",
                pull_request_id: None,
                branch: "main",
                head_sha: &sha,
                workflow_path: ".github/workflows/ci.yml",
                workflow_blobs: &blobs,
                workflow_inputs: None,
                environment: None,
                manual: false,
            },
            &[owner.id],
        )
        .await
        .unwrap();
        let queue = RunQueue::Database {
            pool: pool.clone(),
            config: Arc::new(crate::backend::ci::CiConfig::default()),
            secrets: None,
            context: None,
            notifications: Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                &pool,
            )),
        };
        let (stop_sender, mut stop) = watch::channel(false);
        // "unknown" is not a final state, so the database refuses the result every time.
        let mut recording = tokio::spawn(async move {
            record_result(&queue, "slot-1", &run, "unknown", None, &mut stop).await;
        });
        assert!(
            tokio::time::timeout(Duration::from_secs(3), &mut recording)
                .await
                .is_err(),
            "a result the database refused was given up before the worker stopped"
        );
        stop_sender.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), recording)
            .await
            .expect("the retry stops with the worker")
            .unwrap();
        pool.close().await;
    }
}

//! A CI worker: claims queued runs and executes their workflows with `act`.
use super::config::{podman_socket_path, CiExecutionMode};
use super::process::{self, Stopped};
use super::queue::{ClaimedRun, LogLine, RunQueue};
use super::runner_labels::runner_labels;
use super::workers::CiWorkers;
use crate::backend::db::{self, models::CiRun, DbPool};
use crate::backend::repositories::store::RepoStore;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tracing::warn;

const IDLE_POLL: Duration = Duration::from_secs(3);
const HEARTBEAT: Duration = Duration::from_secs(10);
const RUN_LABEL_KEY: &str = "com.codeyard.ci.run";
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);

/// A running run without a heartbeat for this long has lost its worker.
const STALE_AFTER_SECONDS: i64 = 90;

/// Attempts to record a final state, with doubling waits of 1 to 8 seconds between them.
const FINISH_ATTEMPTS: u32 = 5;

/// How long a claim still in flight at shutdown may run, for a database or a remote queue.
/// A claim that has not returned by then is dropped, so shutdown stays prompt.
const SHUTDOWN_CLAIM_GRACE: Duration = Duration::from_secs(2);

/// Output lines reach the core in batches of at most this many lines (the core's limit per
/// call) or bytes. A waiting batch goes out after `LOG_BATCH_INTERVAL` at the latest, and the
/// command may print `LOG_QUEUE_LINES` lines ahead of the sender before it waits.
const LOG_BATCH_LINES: usize = 64;
const LOG_BATCH_BYTES: usize = 64 * 1024;
const LOG_BATCH_INTERVAL: Duration = Duration::from_millis(250);
const LOG_QUEUE_LINES: usize = 256;

/// Claims and executes runs until `stop` is set.
pub(super) async fn run(service: CiWorkers, worker: String, mut stop: watch::Receiver<bool>) {
    let mut backoff = IDLE_POLL;
    while !*stop.borrow() {
        // A claim may commit a run as shutdown starts. Its grace lets that run reach `execute`,
        // which cancels and finishes it, instead of leaving it running until the stale sweep.
        // A remote claim still dialing when shutdown starts gives up at once, before its request
        // is sent. Once the request is sent, the claim is awaited under the same grace, because
        // the core may already have committed the run for this worker.
        // The claim waits on its own receiver, so the waits below still see the stop signal.
        let mut stopping = stop.clone();
        let mut claim_stop = stop.clone();
        // A database worker claims only while its node is an active actions-worker with its own
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
                                && node.roles.iter().any(|role| role == "actions-worker")
                        }),
                    "not an active actions-worker"
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
                    // A slot with containers it could not remove must not claim unrelated work.
                    warn!(
                        %worker,
                        run = %run_id,
                        "CI worker slot stopped after container cleanup failure"
                    );
                    break;
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
pub(super) async fn fail_stale_runs(pool: DbPool, mut stop: watch::Receiver<bool>) {
    while !*stop.borrow() {
        if let Err(error) = db::ci::fail_stale_ci_runs(&pool, STALE_AFTER_SECONDS).await {
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
    let ownership = if service.config.mode == CiExecutionMode::Podman {
        match ContainerOwnership::create(
            &service.cleanup_dir,
            &service.config.podman_socket,
            run.run_id,
        ) {
            Ok(ownership) => Some(ownership),
            Err(error) => {
                warn!(%error, "could not persist CI container ownership");
                record_result(
                    service,
                    worker,
                    run,
                    "failed",
                    Some("CI container recovery is unavailable"),
                )
                .await;
                return false;
            }
        }
    } else {
        None
    };
    let (cancel_tx, cancel) = watch::channel(false);
    let heartbeat = async {
        let mut shutdown = false;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(HEARTBEAT) => {}
                _ = stop.changed(), if !shutdown => {
                    shutdown = true;
                    let _ = cancel_tx.send(true);
                    continue;
                }
            }
            match service.queue.heartbeat(worker, run.id).await {
                Ok(Some(false)) => {}
                Ok(Some(true)) | Ok(None) => {
                    let _ = cancel_tx.send(true);
                }
                Err(error) => warn!(%error, "CI heartbeat failed"),
            }
        }
    };
    let (cleaned, cleanup) = oneshot::channel();
    let result = tokio::select! {
        result = run_workflows(service, worker, &claim, cancel, cleaned) => result,
        _ = heartbeat => unreachable!("heartbeat loop never ends"),
    };
    let shutting_down = *stop.borrow();
    let (status, message) = match &result {
        Ok(true) => ("success", None),
        Ok(false) => ("failed", Some("A workflow job failed".to_owned())),
        Err(error) => match error.downcast_ref::<Stopped>() {
            Some(Stopped::Cancelled) if shutting_down => (
                "failed",
                Some("The CI worker shut down during the run".to_owned()),
            ),
            Some(Stopped::Cancelled) => ("cancelled", Some("Cancelled".to_owned())),
            Some(Stopped::TimedOut) => {
                ("failed", Some("The run exceeded its time limit".to_owned()))
            }
            None => {
                let message = format!("CI could not run: {error:#}");
                let _ = send_log_lines(
                    service,
                    worker,
                    run,
                    &[LogLine {
                        stage: "setup".into(),
                        line: message.clone(),
                        level: "error".into(),
                    }],
                )
                .await;
                ("failed", Some(message))
            }
        },
    };
    // act leaves its containers behind when it is killed. A slot that cannot remove them
    // stops claiming work rather than piling up containers.
    let mut reuse_slot = true;
    if let Some(ownership) = ownership {
        if let Err(error) = ownership.remove().await {
            warn!(%error, run = %run.run_id, "could not remove CI containers");
            reuse_slot = false;
        }
    }
    record_result(service, worker, run, status, message.as_deref()).await;
    // Do not let a fast runner accumulate unfinished directory removals in this slot.
    let _ = cleanup.await;
    reuse_slot
}

/// Sends the final state of a run. Finishing is idempotent (the first final state wins), so
/// a result whose reply was lost is simply sent again. Without retries, a brief network
/// failure would leave the run to the stale sweep, which records it as abandoned.
async fn record_result(
    service: &CiWorkers,
    worker: &str,
    run: &CiRun,
    status: &str,
    message: Option<&str>,
) {
    let mut delay = Duration::from_secs(1);
    for attempt in 1..=FINISH_ATTEMPTS {
        match service.queue.finish(worker, run.id, status, message).await {
            Ok(_) => return,
            Err(error) => warn!(%error, attempt, run = %run.run_id, "could not record CI result"),
        }
        if attempt < FINISH_ATTEMPTS {
            tokio::time::sleep(delay).await;
            delay *= 2;
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
    cleaned: oneshot::Sender<()>,
) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(service.config.run_timeout_seconds.into());
    let run = &claim.run;
    let store = service.queue.store(claim)?;
    // A run still preparing when shutdown starts stops here, not after the checkout, and
    // the checkout counts against the run's time limit.
    let workspace = tokio::select! {
        prepared = Workspace::prepare(&store, run, service.cleanups.clone(), cleaned) => prepared?,
        _ = process::cancelled(cancel.clone()) => return Err(Stopped::Cancelled.into()),
        _ = tokio::time::sleep(deadline.saturating_duration_since(Instant::now())) => {
            return Err(Stopped::TimedOut.into());
        }
    };
    let secret_file = workspace.write_secrets(&claim.secrets)?;
    workspace.write_event(claim)?;
    let podman_label =
        (service.config.mode == CiExecutionMode::Podman).then(|| run_container_label(run.run_id));
    for workflow in run.workflow_path.split(',').filter(|path| !path.is_empty()) {
        let definition = workspace.definition(workflow);
        let labels = runner_labels(
            &std::fs::read(&definition)?,
            service.config.mode == CiExecutionMode::Local,
        )?;
        if service.config.mode == CiExecutionMode::Podman {
            if let Some(label) = labels
                .iter()
                .find(|label| !service.config.act_platform_map.contains_key(*label))
            {
                bail!("no runner image for `runs-on: {label}`; add it to CI_ACT_PLATFORM_MAP");
            }
            add_container_label(&definition, podman_label.as_deref().unwrap())?;
        }
        let command = act_command(
            service,
            claim,
            &workspace,
            &definition,
            &labels,
            secret_file.as_deref(),
            podman_label.as_deref(),
        );
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), Stopped::TimedOut);
        let stage = workflow.to_owned();
        let (lines_tx, lines_rx) = mpsc::channel(LOG_QUEUE_LINES);
        let running = async {
            let status = process::run(command, remaining, cancel.clone(), |line, stderr| {
                let level = if stderr { log_level(&line) } else { "info" };
                let line = LogLine {
                    stage: stage.clone(),
                    line,
                    level: level.to_owned(),
                };
                let lines_tx = lines_tx.clone();
                async move {
                    lines_tx
                        .send(line)
                        .await
                        .map_err(|_| anyhow::anyhow!("CI log batches stopped"))
                }
            })
            .await;
            // The batches end once the last sender is gone, which is when the output ends.
            drop(lines_tx);
            status
        };
        let (status, sent) =
            tokio::join!(running, send_log_batches(service, worker, run, lines_rx));
        sent?;
        if !status?.success() {
            return Ok(false);
        }
    }
    Ok(true)
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
    Some(if run.branch.starts_with("refs/") {
        run.branch.clone()
    } else {
        format!("refs/heads/{}", run.branch)
    })
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
    let home = workspace.root().join("home");
    let mut command = process::command(&config.act_binary);
    #[cfg(target_os = "linux")]
    {
        let parent = std::process::id();
        // A dead server must not leave act submitting new containers during recovery.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
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
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("DOCKER_CONFIG", home.join(".docker"))
        .env("GITSERV_RUN_ID", run.run_id.to_string())
        .env("GITSERV_PIPELINE", &run.pipeline)
        .env("GITSERV_REPO_OWNER", &claim.owner)
        .env("GITSERV_REPO_NAME", &claim.repository_name)
        .env("GITSERV_BRANCH", &run.branch)
        .env("GITSERV_HEAD_SHA", run.head_sha.as_deref().unwrap_or(""));
    if let Some(environment) = &run.environment {
        command.env("GITSERV_ENVIRONMENT", environment);
    }
    // What `github.repository`, `github.actor` and `github.ref` show in the workflow.
    command.arg("--env").arg(format!(
        "GITHUB_REPOSITORY={}/{}",
        claim.owner, claim.repository_name
    ));
    if !claim.actor.is_empty() {
        command.arg("--actor").arg(&claim.actor);
    }
    if claim.pull_request.is_some() || claim.run.event == "workflow_dispatch" {
        command.arg("--eventpath").arg(workspace.event_path());
    }
    if let Some(reference) = github_ref(claim) {
        command.arg("--env").arg(format!("GITHUB_REF={reference}"));
    }
    match config.mode {
        CiExecutionMode::Local => {
            for label in labels {
                command.arg("-P").arg(format!("{label}=-self-hosted"));
            }
        }
        CiExecutionMode::Podman => {
            for (label, image) in &config.act_platform_map {
                command.arg("-P").arg(format!("{label}={image}"));
            }
            let socket = podman_socket_path(&config.podman_socket)
                .expect("CiConfig contains a validated Podman socket");
            command.env("DOCKER_HOST", format!("unix://{socket}"));
            if let Some(label) = podman_label {
                let options = run_container_options(label);
                // Direct act gets the option from argv. The shipped wrapper combines the
                // same value with its resource limits instead of replacing either one.
                command
                    .env("GITSERV_CONTAINER_OPTIONS", &options)
                    .arg("--container-options")
                    .arg(options);
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

fn run_container_options(label: &str) -> String {
    format!("--label {label}")
}

fn container_filters(label: &str) -> String {
    serde_json::to_string(&BTreeMap::from([("label", [label])]))
        .expect("the static container filter is serializable")
}

fn add_container_label(path: &Path, label: &str) -> Result<()> {
    let mut workflow = super::workflows::parse_yaml(&std::fs::read(path)?)
        .context("could not prepare workflow container cleanup")?;
    let Some(jobs) = workflow.get_mut("jobs").and_then(Value::as_mapping_mut) else {
        bail!("workflow jobs must be a mapping");
    };
    let option = run_container_options(label);
    for job in jobs.values_mut() {
        let Some(job) = job.as_mapping_mut() else {
            continue;
        };
        if let Some(container) = job.get_mut(Value::String("container".into())) {
            append_container_option(container, &option, true)?;
        }
        if let Some(services) = job
            .get_mut(Value::String("services".into()))
            .and_then(Value::as_mapping_mut)
        {
            for service in services.values_mut() {
                append_container_option(service, &option, false)?;
            }
        }
    }
    std::fs::write(
        path,
        serde_yaml::to_string(&workflow).context("could not prepare workflow container cleanup")?,
    )?;
    Ok(())
}

fn append_container_option(spec: &mut Value, option: &str, image_shorthand: bool) -> Result<()> {
    if image_shorthand {
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
    }
    let Some(mapping) = spec.as_mapping_mut() else {
        bail!("workflow container configuration must be a mapping");
    };
    let key = Value::String("options".into());
    match mapping.get_mut(&key) {
        None => {
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
}

struct ContainerOwnership {
    // An exclusive file lock is the local execution claim, including while the core is offline.
    _lock: std::fs::File,
    path: PathBuf,
    record: ContainerRecord,
}

impl ContainerOwnership {
    fn create(directory: &Path, socket: &str, run_id: uuid::Uuid) -> Result<Self> {
        let record = ContainerRecord {
            socket: socket.to_owned(),
            run_id,
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
        Ok(Self {
            _lock: file,
            path,
            record,
        })
    }

    async fn remove(self) -> Result<()> {
        remove_run_containers(
            &self.record.socket,
            &run_container_label(self.record.run_id),
        )
        .await?;
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
            ContainerOwnership {
                _lock: file,
                path,
                record,
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
struct ContainerSummary {
    #[serde(rename = "Id")]
    id: String,
}

/// Force-removes every container carrying the run's label through the Podman socket.
async fn remove_run_containers(socket: &str, label: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .unix_socket(podman_socket_path(socket)?)
        .timeout(CLEANUP_TIMEOUT)
        .build()?;
    let filters = container_filters(label);
    let containers: Vec<ContainerSummary> = client
        .get("http://localhost/v1.40/containers/json")
        .query(&[("all", "true"), ("filters", filters.as_str())])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for container in containers {
        let response = client
            .delete(format!(
                "http://localhost/v1.40/containers/{}",
                container.id
            ))
            // Anonymous volumes, which images such as postgres declare, go with the container.
            .query(&[("force", "true"), ("v", "true")])
            .send()
            .await?;
        if response.status() != reqwest::StatusCode::NOT_FOUND {
            response.error_for_status()?;
        }
    }
    Ok(())
}

/// Stores output lines. The Actions pages read the stored lines.
async fn send_log_lines(
    service: &CiWorkers,
    worker: &str,
    run: &CiRun,
    lines: &[LogLine],
) -> Result<()> {
    let result = service.queue.logs(worker, run.id, lines).await;
    // A disconnected core must not stop a job; its normal deadline still applies.
    if matches!(&service.queue, super::queue::RunQueue::Remote { .. }) {
        if let Err(error) = result {
            warn!(%error, "could not send CI log lines");
        }
        Ok(())
    } else {
        result
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
                bytes += line.stage.len() + line.line.len();
                batch.push(line);
                if batch.len() < LOG_BATCH_LINES && bytes < LOG_BATCH_BYTES {
                    continue;
                }
            }
            _ = tick.tick(), if !batch.is_empty() => {}
        }
        send_log_lines(service, worker, run, &batch).await?;
        batch.clear();
        bytes = 0;
    }
    if batch.is_empty() {
        return Ok(());
    }
    send_log_lines(service, worker, run, &batch).await
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
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let temp = self.temp.take().unwrap();
        let cleaned = self.cleaned.take();
        let mut cleanups = self.cleanups.lock().unwrap_or_else(|e| e.into_inner());
        while cleanups.try_join_next().is_some() {}
        cleanups.spawn_blocking(move || {
            if let Err(error) = temp.close() {
                warn!(%error, "could not remove CI workspace");
            }
            if let Some(cleaned) = cleaned {
                let _ = cleaned.send(());
            }
        });
    }
}

impl Workspace {
    async fn prepare(
        store: &RepoStore,
        run: &CiRun,
        cleanups: Arc<Mutex<JoinSet<()>>>,
        cleaned: oneshot::Sender<()>,
    ) -> Result<Self> {
        let workspace = Self {
            temp: Some(tempfile::tempdir().context("could not create a CI workspace")?),
            cleanups,
            cleaned: Some(cleaned),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(workspace.root(), std::fs::Permissions::from_mode(0o700))
                .await
                .context("could not secure the CI workspace")?;
        }
        for dir in ["home/.docker", "definitions"] {
            tokio::fs::create_dir_all(workspace.root().join(dir)).await?;
        }
        let sha = run.head_sha.as_deref().context("the run has no commit")?;
        store.checkout(&workspace.checkout(), sha).await?;
        // Run exactly the workflow files that were checked when the run was queued.
        let blobs = run
            .workflow_blobs
            .as_object()
            .context("the run has no workflow files")?;
        for (path, id) in blobs {
            let id = id.as_str().context("bad workflow blob id")?;
            let content = store.blob_by_id(id).await?;
            tokio::fs::write(workspace.definition(path), content).await?;
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

    fn event_path(&self) -> PathBuf {
        self.root().join("event.json")
    }

    /// Writes `github.event` for act's `--eventpath`: the pull request, or the manual inputs.
    fn write_event(&self, claim: &ClaimedRun) -> Result<()> {
        let event = if let Some(pull) = &claim.pull_request {
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

    /// Writes repository secrets in act's `--secret-file` (dotenv) format.
    fn write_secrets(&self, secrets: &[(String, String)]) -> Result<Option<PathBuf>> {
        if secrets.is_empty() {
            return Ok(None);
        }
        // Remote claims are untrusted input here. Validate the whole set before the file exists.
        db::secrets::validate_repository_secrets(secrets)?;

        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.root().join("secrets.env");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        for (name, value) in secrets {
            writeln!(file, "{name}={}", dotenv_value(value))?;
        }
        Ok(Some(path))
    }
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn secret_workspace() -> Workspace {
        Workspace {
            temp: Some(tempfile::tempdir().unwrap()),
            cleanups: Arc::new(Mutex::new(JoinSet::new())),
            cleaned: None,
        }
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
        add_container_label(&path, &label).unwrap();
        let yaml: Value = serde_yaml::from_slice(&std::fs::read(path).unwrap()).unwrap();
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
            let _ownership = ContainerOwnership::create(&directory, &socket, run).unwrap();
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
        ])
        .await;
        let live = uuid::Uuid::new_v4();
        let _ownership = ContainerOwnership::create(records.path(), &socket, live).unwrap();
        reconcile_containers(records.path()).await.unwrap();
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
        assert!(reconcile_containers(records.path()).await.is_err());
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
    async fn cleanup_removes_only_containers_with_the_run_label() {
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        let label = "com.codeyard.ci.run=01234567-89ab-cdef-0123-456789abcdef";
        let (_directory, socket, mut requests, server) = fake_podman(vec![
            response(200, list_body(&[&first, &second])),
            response(204, Vec::new()),
            response(404, Vec::new()),
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
        for id in [&first, &second] {
            let (method, target) = requests.recv().await.unwrap();
            assert_eq!(method, "DELETE");
            assert!(target.starts_with(&format!("/v1.40/containers/{id}?")));
            let query = decoded_query(&target);
            assert_eq!(query.get("force").map(String::as_str), Some("true"));
            assert_eq!(query.get("v").map(String::as_str), Some("true"));
        }

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
        let workspace = Workspace::prepare(&store, &run, cleanups.clone(), cleaned)
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
            secrets: Vec::new(),
            pull_request: None,
            storage_peer: None,
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
            secrets: Vec::new(),
            pull_request: Some(crate::backend::ci::queue::PullRequestEvent {
                number: 7,
                title: "Add a feature".to_owned(),
                head_ref: "feature".to_owned(),
                base_ref: "main".to_owned(),
                base_sha: "b".repeat(40),
            }),
            storage_peer: None,
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
    }
}

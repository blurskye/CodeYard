//! Worker execution and shutdown for this node.
use crate::queue::RunQueue;
use anyhow::ensure;
use anyhow::Result;
use codeyard_ci::config::CiConfig;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use uuid::Uuid;

/// How long shutdown lets workers record their results before their tasks are aborted.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// How long startup waits for the first container recovery pass. The background pass
/// retries whatever this one does not finish.
const STARTUP_RECOVERY_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct CiWorkers {
    pub queue: RunQueue,
    pub config: Arc<CiConfig>,
    pub queued: Arc<Notify>,
    pub(super) cleanups: Arc<Mutex<JoinSet<()>>>,
    pub(super) cleanup_dir: PathBuf,
    run_workers: bool,
    configuration_error: Option<String>,
    workers: Arc<Workers>,
    _log_objects: Option<Arc<dyn codeyard_db_core::ci_logs::LogObjects>>,
}

struct Workers {
    stop: watch::Sender<bool>,
    tasks: Mutex<JoinSet<()>>,
}

impl CiWorkers {
    pub fn new(
        queue: RunQueue,
        config: Arc<CiConfig>,
        run_workers: bool,
        state_dir: &Path,
    ) -> Self {
        Self::with_wakeup(
            queue,
            config,
            run_workers,
            Arc::new(Notify::new()),
            state_dir,
        )
    }

    pub fn with_wakeup(
        queue: RunQueue,
        config: Arc<CiConfig>,
        run_workers: bool,
        queued: Arc<Notify>,
        state_dir: &Path,
    ) -> Self {
        let (work_root, configuration_error) = match crate::runner_credentials::work_root(state_dir)
        {
            Ok(root) => (root, None),
            Err(error) => (state_dir.to_owned(), Some(error.to_string())),
        };
        let log_objects = match &queue {
            RunQueue::Database { pool, .. } => pool.log_objects().ok(),
            RunQueue::Remote { .. } => None,
        };
        Self {
            _log_objects: log_objects,
            queue,
            config,
            run_workers,
            queued,
            cleanups: Arc::new(Mutex::new(JoinSet::new())),
            cleanup_dir: work_root.join("ci-containers"),
            configuration_error,
            workers: Arc::new(Workers {
                stop: watch::channel(false).0,
                tasks: Mutex::new(JoinSet::new()),
            }),
        }
    }

    /// Database nodes sweep abandoned runs even when they execute no jobs.
    pub async fn startup(&self) -> Result<()> {
        ensure!(
            self.configuration_error.is_none(),
            "Invalid saved runner configuration"
        );
        let workers = &self.workers;
        let recover = self.run_workers || self.cleanup_dir.is_dir();
        // Workers claim only after abandoned containers are gone. When the startup pass does not
        // finish, each worker recovers again before its first claim.
        let mut recovered = true;
        if recover {
            codeyard_support::fs::private_dir(&self.cleanup_dir)?;
            // A Podman socket that accepts connections but never answers must not hold up the
            // listeners. Records the pass does not reach stay on disk for the background pass.
            match tokio::time::timeout(
                STARTUP_RECOVERY_WAIT,
                crate::worker::reconcile_containers(&self.cleanup_dir),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    recovered = false;
                    tracing::warn!(%error, "CI container recovery will be retried")
                }
                Err(_) => {
                    recovered = false;
                    tracing::warn!("CI container recovery did not finish during startup")
                }
            }
        }
        // The node name, as the rest of the node setup reads it. An empty value counts
        // as unset, because compose passes an unset variable on as an empty one.
        let node = match std::env::var("NODE_NAME")
            .ok()
            .filter(|name| !name.trim().is_empty())
        {
            Some(name) => name,
            None => codeyard_cluster_core::settings::hostname()?,
        };
        let node = codeyard_cluster_core::pairing::normalize_name(&node)?;
        let mut tasks = workers.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let RunQueue::Database { .. } = &self.queue {
            tasks.spawn(crate::worker::fail_stale_runs(
                self.queue.clone(),
                workers.stop.subscribe(),
            ));
        }
        if recover {
            tasks.spawn(crate::worker::recover_containers(
                self.cleanup_dir.clone(),
                workers.stop.subscribe(),
            ));
        }
        if !self.run_workers {
            return Ok(());
        }
        // A storage node authorizes a checkout by the run's worker id starting with the
        // caller's node key, as it does for database-free workers.
        let owner = match &self.queue {
            RunQueue::Database {
                context: Some(context),
                ..
            } => format!("{}/", context.key.public_key()),
            _ => String::new(),
        };
        for slot in 0..self.config.max_active_runs {
            let id = format!("{owner}{node}/{}/{slot}", Uuid::new_v4().simple());
            tasks.spawn(crate::worker::run(
                self.clone(),
                id,
                workers.stop.subscribe(),
                !recovered,
            ));
        }
        tracing::info!(
            workers = self.config.max_active_runs,
            mode = self.config.mode.as_str(),
            "CI workers started"
        );
        Ok(())
    }

    /// Stops new claims and signals running jobs without waiting for them.
    pub fn request_stop(&self) {
        self.workers.stop.send_replace(true);
    }

    /// Completes when membership refusal or shutdown retires this worker process.
    pub async fn stopped(&self) {
        let mut stop = self.workers.stop.subscribe();
        let _ = stop.wait_for(|stopped| *stopped).await;
    }

    /// Cancels running jobs and waits briefly for results and workspace cleanup.
    pub async fn shutdown(&self) -> Result<()> {
        self.request_stop();
        let workers = &self.workers;
        let mut tasks =
            std::mem::take(&mut *workers.tasks.lock().unwrap_or_else(|e| e.into_inner()));
        if tokio::time::timeout(SHUTDOWN_GRACE, async {
            while tasks.join_next().await.is_some() {}
            while std::future::poll_fn(|cx| {
                self.cleanups
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .poll_join_next(cx)
            })
            .await
            .is_some()
            {}
        })
        .await
        .is_err()
        {
            tracing::warn!("CI workers did not stop in time; their runs will be marked failed");
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
        // Blocking deletion cannot be aborted. Keep unfinished handles in the owner so a
        // timed-out shutdown neither waits indefinitely nor detaches the remaining cleanup.
        let mut cleanups = self.cleanups.lock().unwrap_or_else(|e| e.into_inner());
        while cleanups.try_join_next().is_some() {}
        ensure!(cleanups.is_empty(), "CI workspace cleanup is still running");
        Ok(())
    }
}

//! Worker execution and shutdown for this node.
use super::{config::CiConfig, queue::RunQueue, worker};
use anyhow::{ensure, Result};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{watch, Notify},
    task::JoinSet,
};
use uuid::Uuid;

/// How long shutdown lets workers record their results before their tasks are aborted.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// How long startup waits for the first container recovery pass. The background pass
/// retries whatever this one does not finish.
const STARTUP_RECOVERY_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct CiWorkers {
    pub(super) queue: RunQueue,
    pub(super) config: Arc<CiConfig>,
    pub(super) queued: Arc<Notify>,
    pub(super) cleanups: Arc<Mutex<JoinSet<()>>>,
    pub(super) cleanup_dir: PathBuf,
    run_workers: bool,
    workers: Arc<Workers>,
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
        Self {
            queue,
            config,
            run_workers,
            queued,
            cleanups: Arc::new(Mutex::new(JoinSet::new())),
            cleanup_dir: state_dir.join("ci-containers"),
            workers: Arc::new(Workers {
                stop: watch::channel(false).0,
                tasks: Mutex::new(JoinSet::new()),
            }),
        }
    }

    /// Database nodes sweep abandoned runs even when they execute no jobs.
    pub async fn startup(&self) -> Result<()> {
        let workers = &self.workers;
        let recover = self.run_workers || self.cleanup_dir.is_dir();
        if recover {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.cleanup_dir)?;
            // A Podman socket that accepts connections but never answers must not hold up the
            // listeners. Records the pass does not reach stay on disk for the background pass.
            match tokio::time::timeout(
                STARTUP_RECOVERY_WAIT,
                worker::reconcile_containers(&self.cleanup_dir),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "CI container recovery will be retried"),
                Err(_) => tracing::warn!("CI container recovery did not finish during startup"),
            }
        }
        // The node name, as the rest of the node setup reads it. An empty value counts
        // as unset, because compose passes an unset variable on as an empty one.
        let node = std::env::var("NODE_NAME")
            .ok()
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty())
            })
            .unwrap_or_else(|| "node".into());
        let mut tasks = workers.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let RunQueue::Database { pool, .. } = &self.queue {
            tasks.spawn(worker::fail_stale_runs(
                pool.clone(),
                workers.stop.subscribe(),
            ));
        }
        if recover {
            tasks.spawn(worker::recover_containers(
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
            tasks.spawn(worker::run(self.clone(), id, workers.stop.subscribe()));
        }
        tracing::info!(
            workers = self.config.max_active_runs,
            mode = self.config.mode.as_str(),
            "CI workers started"
        );
        Ok(())
    }

    /// Cancels running jobs and waits briefly for results and workspace cleanup.
    pub async fn shutdown(&self) -> Result<()> {
        let workers = &self.workers;
        workers.stop.send_replace(true);
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

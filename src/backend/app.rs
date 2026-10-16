//! Shared application state: database, storage, CI and notification services.
use crate::backend::{
    ci::{CiConfig, CiRuns},
    crypto::SecretEncryptor,
    db::DbPool,
    notifications::NotificationBroadcaster,
    storage::ObjectStore,
};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone)]
pub struct AppState {
    pub pool: DbPool,
    pub repos_path: PathBuf,
    pub cluster: Option<crate::backend::cluster::NodeContext>,
    pub secret_encryptor: Option<Arc<SecretEncryptor>>,
    pub storage: Arc<ObjectStore>,
    pub notification_broadcaster: Arc<NotificationBroadcaster>,
    pub ci_runs: Arc<CiRuns>,
    pub(crate) git_tasks: Arc<crate::backend::repositories::git_sessions::GitTasks>,
    pub(crate) form_reads: Arc<tokio::sync::Semaphore>,
    /// Upload forms held in memory until their commit lands. Only a few run at once.
    pub upload_reads: Arc<tokio::sync::Semaphore>,
    pub(crate) repository_downloads: Arc<tokio::sync::Semaphore>,
}

impl AppState {
    pub fn new(
        pool: DbPool,
        repos_path: PathBuf,
        storage: Arc<ObjectStore>,
        secret_encryptor: Option<Arc<SecretEncryptor>>,
        ci_config: CiConfig,
        cluster: Option<crate::backend::cluster::NodeContext>,
    ) -> Self {
        let notification_broadcaster = Arc::new(NotificationBroadcaster::new(&pool));
        let ci_runs = Arc::new(CiRuns::new(
            pool.clone(),
            Arc::new(ci_config),
            Arc::new(tokio::sync::Notify::new()),
            cluster.clone(),
            notification_broadcaster.clone(),
        ));
        Self {
            pool,
            repos_path,
            cluster,
            secret_encryptor,
            storage,
            notification_broadcaster,
            ci_runs,
            git_tasks: Arc::default(),
            form_reads: Arc::new(tokio::sync::Semaphore::new(8)),
            upload_reads: Arc::new(tokio::sync::Semaphore::new(4)),
            // Each buffered raw file or archive is limited to 32 MiB.
            repository_downloads: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
}

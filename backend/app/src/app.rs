//! Shared application state: database, storage, CI and notification services.
use codeyard_repos_core::ci::runs::CiRuns;
use codeyard_ci::config::CiConfig;
use codeyard_db_core::DbPool;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_storage::ObjectStore;
use codeyard_support::secrets::SecretEncryptor;
use std::path::PathBuf;
use std::sync::Arc;

/// Allows the primary and one replica their full transport admission budgets, plus search time.
const DEFAULT_SEARCH_TIME_BUDGET: std::time::Duration =
    codeyard_cluster::transport::DIAL_TIMEOUT.saturating_mul(7);

const DEFAULT_GRAPHQL_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
pub struct AppState {
    pub pool: DbPool,
    pub repos_path: PathBuf,
    pub cluster: Option<codeyard_repos_core::context::NodeContext>,
    pub secret_encryptor: Option<Arc<SecretEncryptor>>,
    pub storage: Arc<ObjectStore>,
    pub notification_broadcaster: Arc<NotificationBroadcaster>,
    pub ci_runs: Arc<CiRuns>,
    pub oidc_keys: Arc<codeyard_ci::oidc::SigningKeys>,
    pub git_tasks: Arc<codeyard_repos_core::repositories::git_sessions::GitTasks>,
    pub form_reads: Arc<tokio::sync::Semaphore>,
    pub anonymous_form_reads: Arc<tokio::sync::Semaphore>,
    /// Upload forms held in memory until their commit lands. Only a few run at once.
    pub upload_reads: Arc<tokio::sync::Semaphore>,
    pub repository_downloads: Arc<tokio::sync::Semaphore>,
    /// How long one code or commit search page may walk repositories:
    /// `SEARCH_TIME_BUDGET_SECONDS`, default 35.
    pub search_time_budget: std::time::Duration,
    /// How long one GraphQL query may run: `GRAPHQL_QUERY_TIMEOUT_SECONDS`, default 30.
    pub graphql_query_timeout: std::time::Duration,
    pub metrics_token_hash: Option<String>,
    pub roles: codeyard_model::node_roles::NodeRoles,
    pub storage_min_free_bytes: u64,
    pub storage_readiness: Arc<tokio::sync::Semaphore>,
}

impl AppState {
    pub fn new(
        pool: DbPool,
        repos_path: PathBuf,
        storage: Arc<ObjectStore>,
        secret_encryptor: Option<Arc<SecretEncryptor>>,
        ci_config: CiConfig,
        cluster: Option<codeyard_repos_core::context::NodeContext>,
    ) -> Self {
        if cluster.is_none() {
            codeyard_repos_core::repositories::releases::immutable::register_local_identity(
                storage.state_directory(),
                &repos_path,
            );
        }
        let notification_broadcaster = Arc::new(NotificationBroadcaster::new(&pool));
        let ci_runs = Arc::new(CiRuns::new(
            pool.clone(),
            Arc::new(ci_config),
            Arc::new(tokio::sync::Notify::new()),
            cluster.clone(),
            notification_broadcaster.clone(),
        ));
        let oidc_keys = Arc::new(codeyard_ci::oidc::SigningKeys::new(pool.clone()));
        Self {
            metrics_token_hash: std::env::var("METRICS_TOKEN")
                .ok()
                .filter(|token| !token.is_empty() && token.len() <= 4096)
                .map(|token| codeyard_support::token_hash::token_hash(&token)),
            roles: codeyard_model::node_roles::NodeRoles::default(),
            storage_min_free_bytes: 1024 * 1024 * 1024,
            storage_readiness: Arc::new(tokio::sync::Semaphore::new(1)),
            pool,
            oidc_keys,
            repos_path,
            cluster,
            secret_encryptor,
            storage,
            notification_broadcaster,
            ci_runs,
            git_tasks: Arc::default(),
            form_reads: Arc::new(tokio::sync::Semaphore::new(8)),
            anonymous_form_reads: Arc::new(tokio::sync::Semaphore::new(8)),
            upload_reads: Arc::new(tokio::sync::Semaphore::new(4)),
            // Each buffered raw file or archive is limited to 32 MiB.
            repository_downloads: Arc::new(tokio::sync::Semaphore::new(4)),
            search_time_budget: std::env::var("SEARCH_TIME_BUDGET_SECONDS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|seconds| *seconds > 0)
                .map_or(DEFAULT_SEARCH_TIME_BUDGET, std::time::Duration::from_secs),
            graphql_query_timeout: std::env::var("GRAPHQL_QUERY_TIMEOUT_SECONDS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|seconds| *seconds > 0)
                .map_or(
                    DEFAULT_GRAPHQL_QUERY_TIMEOUT,
                    std::time::Duration::from_secs,
                ),
        }
    }
}

impl codeyard_repos_core::request_context::RequestContext for AppState {
    fn pool(&self) -> &DbPool { &self.pool }
    fn cluster(&self) -> Option<&codeyard_repos_core::context::NodeContext> { self.cluster.as_ref() }
}

impl codeyard_repos_pulls::services::RepositoryServices for AppState {
    fn ci_runs(&self) -> &std::sync::Arc<codeyard_repos_core::ci::runs::CiRuns> { &self.ci_runs }
    fn notification_broadcaster(&self) -> &std::sync::Arc<codeyard_notifications::broadcast::NotificationBroadcaster> { &self.notification_broadcaster }
    fn git_tasks(&self) -> &std::sync::Arc<codeyard_repos_core::repositories::git_sessions::GitTasks> { &self.git_tasks }
    fn repository_downloads(&self) -> &std::sync::Arc<tokio::sync::Semaphore> { &self.repository_downloads }
    fn repos_path(&self) -> &std::path::PathBuf { &self.repos_path }
}

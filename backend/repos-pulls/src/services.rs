pub trait RepositoryServices: codeyard_repos_core::request_context::RequestContext {
    fn secret_encryptor(
        &self,
    ) -> &Option<std::sync::Arc<codeyard_support::secrets::SecretEncryptor>>;
    fn ci_runs(&self) -> &std::sync::Arc<codeyard_ci_engine::runs::CiRuns>;
    fn notification_broadcaster(
        &self,
    ) -> &std::sync::Arc<codeyard_notifications::broadcast::NotificationBroadcaster>;
    fn git_tasks(
        &self,
    ) -> &std::sync::Arc<codeyard_repos_core::repositories::git_sessions::GitTasks>;
    fn repository_downloads(&self) -> &std::sync::Arc<tokio::sync::Semaphore>;
    fn repos_path(&self) -> &std::path::PathBuf;
}
impl<T: RepositoryServices> RepositoryServices for std::sync::Arc<T> {
    fn secret_encryptor(
        &self,
    ) -> &Option<std::sync::Arc<codeyard_support::secrets::SecretEncryptor>> {
        (**self).secret_encryptor()
    }
    fn ci_runs(&self) -> &std::sync::Arc<codeyard_ci_engine::runs::CiRuns> {
        (**self).ci_runs()
    }
    fn notification_broadcaster(
        &self,
    ) -> &std::sync::Arc<codeyard_notifications::broadcast::NotificationBroadcaster> {
        (**self).notification_broadcaster()
    }
    fn git_tasks(
        &self,
    ) -> &std::sync::Arc<codeyard_repos_core::repositories::git_sessions::GitTasks> {
        (**self).git_tasks()
    }
    fn repository_downloads(&self) -> &std::sync::Arc<tokio::sync::Semaphore> {
        (**self).repository_downloads()
    }
    fn repos_path(&self) -> &std::path::PathBuf {
        (**self).repos_path()
    }
}

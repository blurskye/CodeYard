pub(crate) use crate::backend::db::models;
pub(crate) mod actions;
pub(crate) mod activity;
pub(crate) mod assignees;
pub(crate) mod audit;
pub(crate) mod ci;
pub(crate) mod cluster;
pub(crate) mod deploy_keys;
pub(crate) mod discussions;
pub(crate) mod email;
pub(crate) mod files;
pub(crate) mod following;
pub(crate) mod issues;
pub(crate) mod labels;
pub(super) mod migrations;
pub(crate) mod milestones;
pub(crate) mod node_config;
pub(crate) mod notifications;
pub(crate) mod organizations;
pub(crate) mod pull_requests;
pub(crate) mod reactions;
pub(crate) mod releases;
pub(crate) mod repositories;
pub(crate) mod repository_stats;
pub(crate) mod secrets;
pub(crate) mod timeline;
pub(crate) mod users;
pub(crate) mod webhooks;

/// Runs `attempt` up to three times, retrying with a short wait when Postgres reports a
/// deadlock (40P01). Any other result is returned as it is.
pub(crate) async fn retry_on_deadlock<T, F, Fut>(mut attempt: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    for tries in 0..3 {
        let result = attempt().await;
        let deadlock = result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<sqlx::Error>())
            .and_then(sqlx::Error::as_database_error)
            .is_some_and(|e| e.code().as_deref() == Some("40P01"));
        if !deadlock || tries == 2 {
            return result;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10 << tries)).await;
    }
    unreachable!()
}

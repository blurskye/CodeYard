/// Runs `attempt` up to three times, retrying with a short wait when Postgres reports a
/// deadlock (40P01) or a serialization failure (40001). A REPEATABLE READ snapshot that takes a
/// row lock gets the latter when another transaction updated that row after the snapshot began.
/// Any other result is returned as it is.
pub async fn retry_on_conflict<T, F, Fut>(mut attempt: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    for tries in 0..3 {
        let result = attempt().await;
        let conflict = result.as_ref().err().is_some_and(|error| {
            error
                .chain()
                .filter_map(|cause| cause.downcast_ref::<sqlx::Error>())
                .filter_map(sqlx::Error::as_database_error)
                .any(|error| matches!(error.code().as_deref(), Some("40P01" | "40001")))
        });
        if !conflict || tries == 2 {
            return result;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10 << tries)).await;
    }
    unreachable!()
}

pub mod action_jobs;

pub mod action_logs;

pub mod action_objects;

pub mod action_settings;

pub mod advisories;

pub mod ci;

pub mod ci_tokens;

pub mod code_security;

pub mod credentials;

pub mod dependabot_alerts;

pub mod dependency_graph;

pub mod deployment_reviews;

pub mod discussions;

pub mod files;

pub mod fine_grained_tokens;

pub mod notifications;

pub mod oauth_apps;

pub mod organizations;

pub mod repositories;

pub mod repository_stats;

pub mod runners;

pub mod secret_scanning;

pub mod secrets;

pub mod users;

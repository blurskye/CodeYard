//! Shared named operation surface; native transactions stay inside db.
//! Actions settings: the site's, from the admin page, and each repository's own.
use crate::backend::db::DbPool;
use anyhow::Result;

/// The site's Actions settings. A missing limit means none; a missing timeout or image leaves
/// the server's own (`CI_RUN_TIMEOUT_SECONDS`, `CI_ACT_RUNNER_IMAGE`).
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct ActionsSettings {
    pub enabled: bool,
    pub run_timeout_minutes: Option<i32>,
    pub job_cpus: Option<i32>,
    pub job_memory_gib: Option<i32>,
    pub runner_image: Option<String>,
}

impl Default for ActionsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            run_timeout_minutes: None,
            job_cpus: None,
            job_memory_gib: None,
            runner_image: None,
        }
    }
}

/// A repository's Actions settings. Its limits apply within the site's.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct RepoActionsSettings {
    pub enabled: bool,
    pub run_timeout_minutes: Option<i32>,
    pub job_cpus: Option<i32>,
    pub job_memory_gib: Option<i32>,
}

impl Default for RepoActionsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            run_timeout_minutes: None,
            job_cpus: None,
            job_memory_gib: None,
        }
    }
}

native_operation!(crate::backend::db::postgres::actions::get_actions_settings,
    crate::backend::db::sqlite::actions::get_actions_settings,
    /// The defaults until an administrator saves the page.
    fn get_actions_settings(pool: &DbPool) -> Result<ActionsSettings>);

native_operation!(crate::backend::db::postgres::actions::save_actions_settings,
    crate::backend::db::sqlite::actions::save_actions_settings,
    /// Returns false when the actor is no longer a signed-in administrator.
    fn save_actions_settings(
    pool: &DbPool,
    actor: i64,
    session: &str,
    settings: &ActionsSettings,
) -> Result<bool>);

native_operation!(crate::backend::db::postgres::actions::get_repo_actions_settings,
    crate::backend::db::sqlite::actions::get_repo_actions_settings,
    fn get_repo_actions_settings(pool: &DbPool, repo_id: i64) -> Result<RepoActionsSettings>);

native_operation!(crate::backend::db::postgres::actions::save_repo_actions_settings,
    crate::backend::db::sqlite::actions::save_repo_actions_settings,
    /// Refused with `RepositorySettingsRefusal::Denied` unless the actor administers the
    /// repository and it is not archived.
    fn save_repo_actions_settings(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    settings: &RepoActionsSettings,
) -> Result<()>);

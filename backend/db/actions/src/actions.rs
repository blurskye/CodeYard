use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

/// The site's Actions settings. A missing limit means none; a missing timeout or image leaves
/// the server's own (`CI_RUN_TIMEOUT_SECONDS`, `CI_ACT_RUNNER_IMAGE`).
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct ActionsSettings {
    pub default_workflow_permissions: String,
    pub default_engine: String,
    pub codeyard_actions_enabled: bool,
    pub act_enabled: bool,
    pub enabled: bool,
    pub run_timeout_minutes: Option<i32>,
    pub job_cpus: Option<i32>,
    pub job_memory_gib: Option<i32>,
    pub runner_image: Option<String>,
    pub results_url: Option<String>,
    pub cache_size_gib: Option<i32>,
    pub cache_retention_days: Option<i32>,
    pub artifact_retention_days: Option<i32>,
}

impl ActionsSettings {
    pub(crate) fn tightens_cache_limits(&self, previous: &Self) -> bool {
        self.cache_size_gib.unwrap_or(10) < previous.cache_size_gib.unwrap_or(10)
            || self.cache_retention_days.unwrap_or(7) < previous.cache_retention_days.unwrap_or(7)
    }

    pub fn engine_enabled(&self, engine: &str) -> bool {
        match engine {
            "codeyard-actions" => self.codeyard_actions_enabled,
            "act" => self.act_enabled,
            _ => false,
        }
    }

    pub fn effective_engine<'a>(&'a self, repository: Option<&'a str>) -> &'a str {
        repository
            .filter(|engine| self.engine_enabled(engine))
            .unwrap_or(&self.default_engine)
    }

    pub fn validate_engines(&self) -> Result<()> {
        anyhow::ensure!(
            self.engine_enabled(&self.default_engine),
            "The default engine must be enabled; at least one engine must stay enabled."
        );
        Ok(())
    }

    pub fn cache_size_bytes(&self, repo: &RepoActionsSettings) -> i64 {
        i64::from(CacheLimit::Storage.effective(
            self.cache_size_gib,
            repo.org_cache_size_gib,
            repo.cache_size_gib,
        )) * 1024
            * 1024
            * 1024
    }

    pub fn cache_expiry_days(&self, repo: &RepoActionsSettings) -> i32 {
        CacheLimit::Retention.effective(
            self.cache_retention_days,
            repo.org_cache_retention_days,
            repo.cache_retention_days,
        )
    }
}

impl Default for ActionsSettings {
    fn default() -> Self {
        Self {
            default_workflow_permissions: "restricted".into(),
            default_engine: "codeyard-actions".into(),
            codeyard_actions_enabled: true,
            act_enabled: true,
            enabled: true,
            run_timeout_minutes: None,
            job_cpus: None,
            job_memory_gib: None,
            runner_image: None,
            results_url: None,
            cache_size_gib: None,
            cache_retention_days: None,
            artifact_retention_days: None,
        }
    }
}

/// A repository's Actions settings. Its limits apply within the site's.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct RepoActionsSettings {
    /// Derived from organization policy, never written through repository settings.
    pub org_cache_size_gib: Option<i32>,
    pub org_cache_retention_days: Option<i32>,
    pub engine: Option<String>,
    pub cache_size_gib: Option<i32>,
    pub cache_retention_days: Option<i32>,
    pub enabled: bool,
    pub run_timeout_minutes: Option<i32>,
    pub job_cpus: Option<i32>,
    pub job_memory_gib: Option<i32>,
}

impl Default for RepoActionsSettings {
    fn default() -> Self {
        Self {
            org_cache_size_gib: None,
            org_cache_retention_days: None,
            engine: None,
            cache_size_gib: None,
            cache_retention_days: None,
            enabled: true,
            run_timeout_minutes: None,
            job_cpus: None,
            job_memory_gib: None,
        }
    }
}

native_operation!(codeyard_db_orgs::postgres::organizations::cache::get_cache_settings,
    codeyard_db_orgs::sqlite::organizations::cache::get_cache_settings,
    fn get_org_cache_settings(pool: &DbPool, actor: i64, name: &str) -> Result<codeyard_db_core::queries::organizations::OrganizationOutcome<OrgCacheSettings>>);
native_operation!(codeyard_db_orgs::postgres::organizations::cache::set_cache_limit,
    codeyard_db_orgs::sqlite::organizations::cache::set_cache_limit,
    fn set_org_cache_limit(pool: &DbPool, actor: i64, name: &str, kind: CacheLimit, value: i32) -> Result<codeyard_db_core::queries::organizations::OrganizationOutcome<()>>);

native_operation!(crate::postgres::actions::set_cache_limit,
    crate::sqlite::actions::set_cache_limit,
    fn set_cache_limit(pool: &DbPool, actor: i64, repo: i64, kind: CacheLimit, value: i32) -> Result<()>);

native_operation!(crate::postgres::actions::record_ci_engine,
    crate::sqlite::actions::record_ci_engine,
    fn record_ci_engine(pool: &DbPool, run_id: i64, worker: &str, engine: &str) -> Result<String>);

native_operation!(crate::postgres::actions::get_actions_settings,
    crate::sqlite::actions::get_actions_settings,
    /// The defaults until an administrator saves the page.
    fn get_actions_settings(pool: &DbPool) -> Result<ActionsSettings>);

native_operation!(crate::postgres::actions::save_actions_settings,
    crate::sqlite::actions::save_actions_settings,
    /// Returns false when the actor is no longer a signed-in administrator.
    fn save_actions_settings(
    pool: &DbPool,
    actor: i64,
    session: &str,
    settings: &ActionsSettings,
) -> Result<bool>);

native_operation!(crate::postgres::actions::get_repo_actions_settings,
    crate::sqlite::actions::get_repo_actions_settings,
    fn get_repo_actions_settings(pool: &DbPool, repo_id: i64) -> Result<RepoActionsSettings>);

native_operation!(crate::postgres::actions::set_repo_enabled,
    crate::sqlite::actions::set_repo_enabled,
    /// Changes only the execution switch, preserving native limits and engine selection.
    fn set_repo_enabled(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, repo: i64, enabled: bool) -> Result<()>);

native_operation!(crate::postgres::actions::save_repo_actions_settings,
    crate::sqlite::actions::save_repo_actions_settings,
    /// Refused with `RepositorySettingsRefusal::Denied` unless the actor administers the
    /// repository and it is not archived.
    fn save_repo_actions_settings(
    pool: &DbPool,
    actor_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
    settings: &RepoActionsSettings,
) -> Result<()>);

use codeyard_db_core::queries::actions::CacheLimit;

use codeyard_db_core::queries::actions::OrgCacheSettings;

//! Shared named operation surface; native transactions stay inside db.
//! Actions settings: the site's, from the admin page, and each repository's own.
use crate::backend::db::DbPool;
use anyhow::Result;

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
        i64::from(CacheLimit::Storage.effective(self.cache_size_gib, repo.org_cache_size_gib, repo.cache_size_gib)) * 1024
            * 1024
            * 1024
    }

    pub fn cache_expiry_days(&self, repo: &RepoActionsSettings) -> i32 {
        CacheLimit::Retention.effective(self.cache_retention_days, repo.org_cache_retention_days, repo.cache_retention_days)
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

#[derive(Default, sqlx::FromRow)]
pub struct OrgCacheSettings {
    pub cache_size_gib: Option<i32>,
    pub cache_retention_days: Option<i32>,
}

native_operation!(crate::backend::db::postgres::organizations::cache::get_cache_settings,
    crate::backend::db::sqlite::organizations::cache::get_cache_settings,
    fn get_org_cache_settings(pool: &DbPool, actor: i64, name: &str) -> Result<super::organizations::OrganizationOutcome<OrgCacheSettings>>);
native_operation!(crate::backend::db::postgres::organizations::cache::set_cache_limit,
    crate::backend::db::sqlite::organizations::cache::set_cache_limit,
    fn set_org_cache_limit(pool: &DbPool, actor: i64, name: &str, kind: CacheLimit, value: i32) -> Result<super::organizations::OrganizationOutcome<()>>);

#[derive(Clone, Copy)]
pub enum CacheLimit {
    Storage,
    Retention,
}

impl CacheLimit {
    pub(crate) fn column(self) -> &'static str {
        match self {
            Self::Storage => "cache_size_gib",
            Self::Retention => "cache_retention_days",
        }
    }
    pub(crate) fn effective(self, site: Option<i32>, organization: Option<i32>, repository: Option<i32>) -> i32 {
        site.unwrap_or_else(|| self.default_value()).min(organization.unwrap_or(i32::MAX)).min(repository.unwrap_or(i32::MAX))
    }
    pub(crate) fn default_value(self) -> i32 {
        match self {
            Self::Storage => 10,
            Self::Retention => 7,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CachePolicyError {
    #[error("Repository administration denied")]
    Denied,
    #[error("Cache limit must be positive and no greater than the site limit")]
    Limit,
}

native_operation!(crate::backend::db::postgres::actions::set_cache_limit,
    crate::backend::db::sqlite::actions::set_cache_limit,
    fn set_cache_limit(pool: &DbPool, actor: i64, repo: i64, kind: CacheLimit, value: i32) -> Result<()>);

native_operation!(crate::backend::db::postgres::actions::record_ci_engine,
    crate::backend::db::sqlite::actions::record_ci_engine,
    fn record_ci_engine(pool: &DbPool, run_id: i64, worker: &str, engine: &str) -> Result<String>);

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

native_operation!(crate::backend::db::postgres::actions::set_repo_enabled,
    crate::backend::db::sqlite::actions::set_repo_enabled,
    /// Changes only the execution switch, preserving native limits and engine selection.
    fn set_repo_enabled(pool: &DbPool, actor: impl Into<super::credentials::Actor>, repo: i64, enabled: bool) -> Result<()>);

native_operation!(crate::backend::db::postgres::actions::save_repo_actions_settings,
    crate::backend::db::sqlite::actions::save_repo_actions_settings,
    /// Refused with `RepositorySettingsRefusal::Denied` unless the actor administers the
    /// repository and it is not archived.
    fn save_repo_actions_settings(
    pool: &DbPool,
    actor_id: impl Into<super::credentials::Actor>,
    repo_id: i64,
    settings: &RepoActionsSettings,
) -> Result<()>);

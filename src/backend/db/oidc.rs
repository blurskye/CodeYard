//! Instance signing identity and OIDC claims bound to live job credentials.
use super::DbPool;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Copy)]
pub enum Scope {
    Repository(i64),
    Organization(i64),
}
impl Scope {
    pub fn storage(self) -> (&'static str, &'static str, i64) {
        match self {
            Self::Repository(id) => ("actions_oidc_repo_settings", "repo_id", id),
            Self::Organization(id) => ("actions_oidc_org_settings", "org_id", id),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub use_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_claim_keys: Option<Vec<String>>,
}
impl Settings {
    pub fn from_keys(use_default: bool, keys: &str) -> Self {
        let keys = keys
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|key| !key.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        Self {
            use_default,
            include_claim_keys: (!keys.is_empty()).then_some(keys),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.use_default {
            return Ok(());
        }
        if let Some(keys) = &self.include_claim_keys {
            let mut seen = HashSet::new();
            ensure!(!keys.is_empty() && keys.len() <= 100, Refusal::Invalid);
            for key in keys {
                ensure!(seen.insert(key) && valid_claim(key), Refusal::Invalid);
            }
        }
        Ok(())
    }
}

fn valid_claim(key: &str) -> bool {
    matches!(
        key,
        "repo"
            | "context"
            | "repository"
            | "repository_owner"
            | "repository_id"
            | "repository_owner_id"
            | "actor"
            | "actor_id"
            | "workflow"
            | "workflow_ref"
            | "workflow_sha"
            | "job_workflow_ref"
            | "job_workflow_sha"
            | "event_name"
            | "ref"
            | "ref_type"
            | "sha"
            | "run_id"
            | "run_number"
            | "run_attempt"
            | "runner_environment"
            | "environment"
            | "repository_visibility"
    ) || key
        .strip_prefix("repo_property_")
        .is_some_and(crate::backend::db::organizations::properties::valid_name)
}

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("OIDC administration denied")]
    Denied,
    #[error("Invalid OIDC configuration")]
    Invalid,
    #[error("OIDC property inclusion already exists")]
    Duplicate,
    #[error("OIDC property inclusion not found")]
    Missing,
}

#[derive(Serialize)]
pub struct Inclusion {
    pub custom_property_name: String,
    pub inclusion_source: &'static str,
}

pub struct TokenSettings {
    pub include_claim_keys: Option<Vec<String>>,
    pub properties: BTreeMap<String, String>,
}

native_operation!(crate::backend::db::postgres::oidc_settings::get,
    crate::backend::db::sqlite::oidc_settings::get,
    fn settings(pool: &DbPool, actor: i64, scope: Scope) -> Result<Settings>);
native_operation!(crate::backend::db::postgres::oidc_settings::save,
    crate::backend::db::sqlite::oidc_settings::save,
    fn save_settings(pool: &DbPool, actor: i64, scope: Scope, settings: &Settings) -> Result<()>);
native_operation!(crate::backend::db::postgres::oidc_settings::inclusions,
    crate::backend::db::sqlite::oidc_settings::inclusions,
    fn inclusions(pool: &DbPool, actor: i64, org: i64) -> Result<Vec<Inclusion>>);
native_operation!(crate::backend::db::postgres::oidc_settings::change_inclusion,
    crate::backend::db::sqlite::oidc_settings::change_inclusion,
    fn change_inclusion(pool: &DbPool, actor: i64, org: i64, name: &str, remove: bool) -> Result<()>);
native_operation!(crate::backend::db::postgres::oidc_settings::token_settings,
    crate::backend::db::sqlite::oidc_settings::token_settings,
    fn token_settings(pool: &DbPool, repo: i64) -> Result<TokenSettings>);

#[derive(sqlx::FromRow)]
pub struct Job {
    pub run_id: uuid::Uuid,
    /// The credential's job identity, `root workflow:job/called job`, or `act`.
    pub job: String,
    pub workflow: String,
    pub workflow_name: String,
    pub environment: Option<String>,
}

native_operation!(crate::backend::db::postgres::oidc::key,
    crate::backend::db::sqlite::oidc::key,
    fn key(pool: &DbPool) -> Result<Option<String>>);
native_operation!(crate::backend::db::postgres::oidc::store_key,
    crate::backend::db::sqlite::oidc::store_key,
    fn store_key(pool: &DbPool, pem: &str) -> Result<String>);
native_operation!(crate::backend::db::postgres::oidc::bind_job,
    crate::backend::db::sqlite::oidc::bind_job,
    fn bind_job(pool: &DbPool, hash: &str, workflow: &str, name: &str, environment: Option<&str>) -> Result<()>);
native_operation!(crate::backend::db::postgres::oidc::job,
    crate::backend::db::sqlite::oidc::job,
    fn job(pool: &DbPool, hash: &str) -> Result<Option<Job>>);

//! Deployment requests and append-only status history, independent of CI execution attempts.
use anyhow::Result;
use chrono::{DateTime, Utc};
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Deployment write denied")]
    Denied,
    #[error("Deployment not found")]
    NotFound,
    #[error("Invalid deployment")]
    Invalid,
    #[error("Required commit status checks have not succeeded")]
    Checks,
    #[error("An active deployment cannot be deleted")]
    Active,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Deployment {
    pub id: i64,
    pub repo_id: i64,
    pub creator_id: Option<i64>,
    pub sha: String,
    pub reference: String,
    pub task: String,
    pub payload: String,
    pub original_environment: String,
    pub environment: String,
    pub description: Option<String>,
    pub transient_environment: bool,
    pub production_environment: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Status {
    pub id: i64,
    pub deployment_id: i64,
    pub creator_id: Option<i64>,
    pub state: String,
    pub description: String,
    pub environment: String,
    pub log_url: String,
    pub environment_url: String,
    pub created_at: DateTime<Utc>,
}

pub struct NewDeployment<'a> {
    pub actor: super::credentials::Actor,
    pub repo: i64,
    pub sha: &'a str,
    pub reference: &'a str,
    pub task: &'a str,
    pub payload: &'a serde_json::Value,
    pub environment: &'a str,
    pub description: Option<&'a str>,
    pub transient: bool,
    pub production: bool,
    pub required_contexts: Option<&'a [String]>,
}

fn valid_text(value: &str, limit: usize) -> bool {
    value.chars().count() <= limit && !value.chars().any(char::is_control)
}

impl NewDeployment<'_> {
    pub(crate) fn checks_passed(&self, checks: &[(String, String)]) -> bool {
        match self.required_contexts {
            Some(contexts) => contexts.iter().all(|context| {
                checks
                    .iter()
                    .any(|(name, state)| name.eq_ignore_ascii_case(context) && state == "success")
            }),
            None => checks.iter().all(|(_, state)| state == "success"),
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            codeyard_git::is_full_commit_id(self.sha)
                && !self.reference.is_empty()
                && valid_text(self.reference, 1024)
                && !self.task.is_empty()
                && valid_text(self.task, 255)
                && !self.environment.trim().is_empty()
                && valid_text(self.environment, 255)
                && self.description.is_none_or(|value| valid_text(value, 1024))
                && (self.payload.is_object() || self.payload.is_string())
                && self.payload.to_string().len() <= 65536
                && self
                    .required_contexts
                    .is_none_or(|contexts| contexts.len() <= 100
                        && contexts
                            .iter()
                            .all(|context| !context.is_empty() && valid_text(context, 255))),
            Refusal::Invalid
        );
        Ok(())
    }
}

#[derive(Default, Deserialize)]
pub struct Filter {
    pub sha: Option<String>,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub task: Option<String>,
    pub environment: Option<String>,
}

#[derive(Deserialize)]
pub struct NewStatus {
    pub state: String,
    #[serde(default)]
    pub description: String,
    pub environment: Option<String>,
    pub target_url: Option<String>,
    pub log_url: Option<String>,
    #[serde(default)]
    pub environment_url: String,
    pub auto_inactive: Option<bool>,
}

impl NewStatus {
    pub(crate) fn validate(&self) -> Result<()> {
        let valid_url = |value: &str| {
            value.is_empty()
                || (value.len() <= 4096
                    && url::Url::parse(value)
                        .is_ok_and(|url| matches!(url.scheme(), "http" | "https")))
        };
        anyhow::ensure!(
            matches!(
                self.state.as_str(),
                "error" | "failure" | "inactive" | "in_progress" | "queued" | "pending" | "success"
            ) && valid_text(&self.description, 140)
                && self
                    .environment
                    .as_ref()
                    .is_none_or(|value| !value.trim().is_empty() && valid_text(value, 255))
                && self.log_url.as_deref().is_none_or(valid_url)
                && self.target_url.as_deref().is_none_or(valid_url)
                && valid_url(&self.environment_url),
            Refusal::Invalid
        );
        Ok(())
    }
}

native_operation!(crate::backend::db::postgres::deployments::create,
    crate::backend::db::sqlite::deployments::create,
    fn create(pool: &DbPool, input: NewDeployment<'_>) -> Result<Deployment>);
native_operation!(crate::backend::db::postgres::deployments::get,
    crate::backend::db::sqlite::deployments::get,
    fn get(pool: &DbPool, viewer: Option<i64>, repo: i64, id: i64) -> Result<Option<Deployment>>);
native_operation!(crate::backend::db::postgres::deployments::list,
    crate::backend::db::sqlite::deployments::list,
    fn list(pool: &DbPool, viewer: Option<i64>, repo: i64, filter: &Filter, offset: i64, limit: i64) -> Result<Vec<Deployment>>);
native_operation!(crate::backend::db::postgres::deployments::delete,
    crate::backend::db::sqlite::deployments::delete,
    fn delete(pool: &DbPool, actor: impl Into<super::credentials::Actor>, repo: i64, id: i64) -> Result<()>);
native_operation!(crate::backend::db::postgres::deployments::create_status,
    crate::backend::db::sqlite::deployments::create_status,
    /// Every status the call saved, each with its deployment after the change; the new status is last.
    fn create_status(pool: &DbPool, actor: impl Into<super::credentials::Actor>, repo: i64, deployment: i64, input: &NewStatus) -> Result<Vec<(Deployment, Status)>>);
/// Pairs the inactive statuses written for older deployments with those deployments.
pub(in crate::backend::db) fn pair(
    deployments: Vec<Deployment>,
    statuses: Vec<Status>,
) -> Result<Vec<(Deployment, Status)>> {
    let deployments: std::collections::HashMap<i64, Deployment> = deployments
        .into_iter()
        .map(|deployment| (deployment.id, deployment))
        .collect();
    statuses
        .into_iter()
        .map(|status| {
            let deployment = deployments
                .get(&status.deployment_id)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "deployment {} vanished during the status write",
                        status.deployment_id
                    )
                })?;
            Ok((deployment, status))
        })
        .collect()
}
native_operation!(crate::backend::db::postgres::deployments::get_status,
    crate::backend::db::sqlite::deployments::get_status,
    fn get_status(pool: &DbPool, viewer: Option<i64>, deployment: i64, id: i64) -> Result<Option<Status>>);
native_operation!(crate::backend::db::postgres::deployments::statuses,
    crate::backend::db::sqlite::deployments::statuses,
    fn statuses(pool: &DbPool, viewer: Option<i64>, deployment: i64, offset: i64, limit: i64) -> Result<Vec<Status>>);

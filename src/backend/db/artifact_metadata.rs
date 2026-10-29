//! Artifact inventory shares provenance repository associations with stored attestations.
pub mod pages;
use super::DbPool;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Artifact metadata write denied")]
    Denied,
    #[error("Artifact or repository not found")]
    Missing,
    #[error("Invalid artifact metadata")]
    Invalid,
    #[error("A deployment job is already in progress for this cluster")]
    Busy,
    #[error("Artifact inventory is too large for one response")]
    TooLarge,
}

fn text(value: &str, min: usize, max: usize) -> Result<()> {
    anyhow::ensure!(
        (min..=max).contains(&value.chars().count()) && !value.chars().any(char::is_control),
        Refusal::Invalid
    );
    Ok(())
}
fn yes() -> bool {
    true
}
fn deployed() -> String {
    "deployed".into()
}
fn active() -> String {
    "active".into()
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub digest: String,
    pub version: Option<String>,
    pub github_repository: Option<String>,
}
impl Artifact {
    pub fn validate(&self) -> Result<()> {
        text(&self.name, 1, 256)?;
        anyhow::ensure!(
            self.digest.starts_with("sha256:") && super::attestations::valid_digest(&self.digest),
            Refusal::Invalid
        );
        if let Some(version) = &self.version {
            text(version, 0, 100)?;
        }
        if let Some(repo) = &self.github_repository {
            text(repo, 1, 100)?;
            anyhow::ensure!(
                repo.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
                Refusal::Invalid
            );
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Deployment {
    #[serde(flatten)]
    pub artifact: Artifact,
    #[serde(default = "deployed")]
    pub status: String,
    pub deployment_name: String,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    #[serde(default)]
    pub runtime_risks: Vec<String>,
}
impl Deployment {
    pub fn validate(&self) -> Result<()> {
        self.artifact.validate()?;
        text(&self.deployment_name, 1, 256)?;
        anyhow::ensure!(self.tags.len() <= 128, Refusal::Invalid);
        for (key, value) in &self.tags {
            text(key, 1, 256)?;
            text(value, 0, 1024)?;
        }
        anyhow::ensure!(
            matches!(self.status.as_str(), "deployed" | "decommissioned"),
            Refusal::Invalid
        );
        let risks: BTreeSet<_> = self.runtime_risks.iter().collect();
        anyhow::ensure!(
            risks.len() == self.runtime_risks.len()
                && risks.len() <= 4
                && risks.iter().all(|risk| matches!(
                    risk.as_str(),
                    "critical-resource"
                        | "internet-exposed"
                        | "lateral-movement"
                        | "sensitive-data"
                )),
            Refusal::Invalid
        );
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Environment {
    pub logical_environment: String,
    #[serde(default)]
    pub physical_environment: String,
}
impl Environment {
    pub fn validate(&self) -> Result<()> {
        text(&self.logical_environment, 1, 128)?;
        text(&self.physical_environment, 0, 128)
    }
}

#[derive(Deserialize)]
pub struct DeploymentInput {
    #[serde(flatten)]
    pub deployment: Deployment,
    #[serde(flatten)]
    pub environment: Environment,
    #[serde(default)]
    pub cluster: String,
    #[serde(default = "yes")]
    pub return_records: bool,
}
impl DeploymentInput {
    pub fn validate(&self) -> Result<()> {
        self.deployment.validate()?;
        self.environment.validate()?;
        text(&self.cluster, 0, 128)?;
        anyhow::ensure!(self.deployment.tags.len() <= 5, Refusal::Invalid);
        if let Some(version) = &self.deployment.artifact.version {
            text(version, 1, 100)?;
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ClusterInput {
    #[serde(flatten)]
    pub environment: Environment,
    pub deployments: Vec<Deployment>,
    #[serde(default)]
    pub partial_success: bool,
    #[serde(default = "yes")]
    pub return_records: bool,
}
impl ClusterInput {
    pub fn validate(&self, cluster: &str, maximum: usize) -> Result<()> {
        self.environment.validate()?;
        text(cluster, 1, 128)?;
        anyhow::ensure!(
            cluster
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
                && self.deployments.len() <= maximum,
            Refusal::Invalid
        );
        let mut names = BTreeSet::new();
        for deployment in &self.deployments {
            deployment.validate()?;
            anyhow::ensure!(names.insert(&deployment.deployment_name), Refusal::Invalid);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
pub struct StorageInput {
    #[serde(flatten)]
    pub artifact: Artifact,
    pub artifact_url: Option<String>,
    pub path: Option<String>,
    pub registry_url: String,
    pub repository: Option<String>,
    #[serde(default = "active")]
    pub status: String,
    #[serde(default = "yes")]
    pub return_records: bool,
}
impl StorageInput {
    pub(crate) fn identity(&self, repo: i64) -> String {
        use sha2::{Digest, Sha256};
        let value = json!([
            repo,
            self.artifact.digest,
            self.registry_url,
            self.repository.as_deref().unwrap_or(""),
            self.path.as_deref().unwrap_or("")
        ]);
        hex::encode(Sha256::digest(value.to_string().as_bytes()))
    }

    pub fn validate(&self) -> Result<()> {
        self.artifact.validate()?;
        if let Some(version) = &self.artifact.version {
            text(version, 1, 100)?;
        }
        fn https(value: &str, maximum: usize) -> Result<()> {
            text(value, 1, maximum)?;
            let url = url::Url::parse(value).map_err(|_| Refusal::Invalid)?;
            anyhow::ensure!(
                url.scheme() == "https"
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none(),
                Refusal::Invalid
            );
            Ok(())
        }
        https(&self.registry_url, 256)?;
        if let Some(url) = &self.artifact_url {
            https(url, 152)?;
        }
        if let Some(path) = &self.path {
            text(path, 0, 512)?;
        }
        if let Some(repository) = &self.repository {
            text(repository, 0, 128)?;
        }
        anyhow::ensure!(
            matches!(self.status.as_str(), "active" | "eol" | "deleted"),
            Refusal::Invalid
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct JobInput {
    pub input: ClusterInput,
}

#[derive(Clone, Copy)]
pub struct Actor {
    pub user: i64,
    pub token: i64,
    /// A job token never borrows its bot account's grants on other repositories.
    pub repo: Option<i64>,
}

#[derive(sqlx::FromRow)]
pub struct Record {
    pub id: i64,
    pub repo_id: i64,
    pub kind: String,
    pub digest: String,
    pub name: String,
    pub version: Option<String>,
    pub document: String,
    pub attestation_id: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}
impl Record {
    pub fn document(&self) -> Result<Value> {
        let mut value: Value = serde_json::from_str(&self.document)?;
        value["id"] = json!(self.id);
        value["digest"] = json!(self.digest);
        value["created_at"] = json!(self.created_at);
        value["updated_at"] = json!(self.updated_at);
        if self.kind == "storage" {
            value["name"] = json!(self.name);
            if let Some(version) = &self.version {
                value["version"] = json!(version);
            }
        } else {
            value["attestation_id"] = json!(self.attestation_id);
        }
        Ok(value)
    }
}

pub(crate) fn append_record(rows: &mut Vec<Record>, row: Record, bytes: &mut usize) -> Result<()> {
    *bytes +=
        row.document.len() + row.name.len() + row.version.as_ref().map_or(0, String::len) + 512;
    anyhow::ensure!(
        *bytes <= 16 * 1024 * 1024 && rows.len() < 10_000,
        Refusal::TooLarge
    );
    rows.push(row);
    Ok(())
}

#[derive(Default)]
pub struct Mutation {
    pub records: Vec<Record>,
    pub errors: Vec<Value>,
}

#[derive(sqlx::FromRow)]
pub struct Job {
    pub id: i64,
    pub org_id: i64,
    pub cluster: String,
    pub actor_id: i64,
    pub token_id: i64,
    pub scoped_repo: Option<i64>,
    pub input: String,
    pub status: String,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub total_count: Option<i64>,
    pub errors: String,
}

native_operation!(crate::backend::db::postgres::artifact_metadata::deployment, crate::backend::db::sqlite::artifact_metadata::deployment,
    fn deployment(pool: &DbPool, actor: Actor, org: i64, input: &DeploymentInput) -> Result<Mutation>);
native_operation!(crate::backend::db::postgres::artifact_metadata::storage, crate::backend::db::sqlite::artifact_metadata::storage,
    fn storage(pool: &DbPool, actor: Actor, org: i64, input: &StorageInput) -> Result<Mutation>);
native_operation!(crate::backend::db::postgres::artifact_metadata::cluster, crate::backend::db::sqlite::artifact_metadata::cluster,
    fn cluster(pool: &DbPool, actor: Actor, org: i64, cluster: &str, input: &ClusterInput) -> Result<Mutation>);
native_operation!(crate::backend::db::postgres::artifact_metadata::list, crate::backend::db::sqlite::artifact_metadata::list,
    fn list(pool: &DbPool, actor: Actor, org: i64, digest: &str, kind: &str) -> Result<Vec<Record>>);
native_operation!(crate::backend::db::postgres::artifact_metadata::enqueue, crate::backend::db::sqlite::artifact_metadata::enqueue,
    fn enqueue(pool: &DbPool, actor: Actor, org: i64, cluster: &str, input: &ClusterInput) -> Result<(i64,Vec<Value>)>);
native_operation!(crate::backend::db::postgres::artifact_metadata::job, crate::backend::db::sqlite::artifact_metadata::job,
    fn job(pool: &DbPool, actor: Actor, org: i64, cluster: &str, id: i64) -> Result<Option<Job>>);
native_operation!(crate::backend::db::postgres::artifact_metadata::process_job, crate::backend::db::sqlite::artifact_metadata::process_job,
    fn process_job(pool: &DbPool, id: Option<i64>) -> Result<bool>);

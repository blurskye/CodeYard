//! Stored alerts and durable scan work. Alert reads require live repository administration.
use super::DbPool;
use crate::backend::repositories::secret_scanning::patterns::{
    Create, Delete, Owner, Pattern, Update,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Default, Serialize, Deserialize)]
pub struct PatternConfig {
    pub version: Option<String>,
    pub settings: BTreeMap<String, String>,
}
#[derive(Deserialize)]
pub struct PatternSetting {
    pub token_type: String,
    pub custom_pattern_version: Option<String>,
    pub push_protection_setting: String,
}
#[derive(Deserialize)]
pub struct ConfigUpdate {
    pub pattern_config_version: Option<String>,
    #[serde(default)]
    pub provider_pattern_settings: Vec<PatternSetting>,
    #[serde(default)]
    pub custom_pattern_settings: Vec<PatternSetting>,
}

native_operation!(super::postgres::secret_scanning::patterns::list,super::sqlite::secret_scanning::patterns::list,
    fn list_patterns(pool:&DbPool,actor:i64,owner:Owner)->Result<Vec<Pattern>>);
native_operation!(super::postgres::secret_scanning::patterns::create,super::sqlite::secret_scanning::patterns::create,
    fn create_patterns(pool:&DbPool,actor:i64,owner:Owner,input:&[Create])->Result<Vec<Pattern>>);
native_operation!(super::postgres::secret_scanning::patterns::update,super::sqlite::secret_scanning::patterns::update,
    fn update_pattern(pool:&DbPool,actor:i64,owner:Owner,id:i64,input:&Update)->Result<Pattern>);
native_operation!(super::postgres::secret_scanning::patterns::delete,super::sqlite::secret_scanning::patterns::delete,
    fn delete_patterns(pool:&DbPool,actor:i64,owner:Owner,input:&[Delete],resolve:bool)->Result<()>);
native_operation!(super::postgres::secret_scanning::patterns::publish,super::sqlite::secret_scanning::patterns::publish,
    fn publish_pattern(pool:&DbPool,actor:i64,owner:Owner,id:i64)->Result<()>);
native_operation!(super::postgres::secret_scanning::patterns::effective,super::sqlite::secret_scanning::patterns::effective,
    fn effective_patterns(pool:&DbPool,repo:i64)->Result<(Vec<Pattern>,Vec<String>)>);
native_operation!(super::postgres::secret_scanning::patterns::configs,super::sqlite::secret_scanning::patterns::configs,
    fn pattern_configs(pool:&DbPool,actor:i64,org:i64)->Result<serde_json::Value>);
native_operation!(super::postgres::secret_scanning::patterns::configure,super::sqlite::secret_scanning::patterns::configure,
    fn configure_patterns(pool:&DbPool,actor:i64,org:i64,input:&ConfigUpdate)->Result<String>);

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Secret scanning access denied")]
    Denied,
    #[error("Secret scanning resource not found")]
    Missing,
    #[error("Invalid secret scanning settings")]
    Invalid,
    #[error("Custom pattern version changed")]
    Version,
    #[error("Pattern configuration version changed")]
    Conflict,
}

#[derive(Clone, Default, sqlx::FromRow)]
pub struct Settings {
    pub enabled: bool,
    pub push_protection: bool,
}

#[derive(Clone, sqlx::FromRow)]
pub struct Alert {
    pub repo_id: i64,
    pub number: i64,
    pub secret_hash: String,
    pub secret_type: String,
    pub display_name: String,
    pub encrypted_secret: Vec<u8>,
    pub nonce: Vec<u8>,
    pub assigned_to: Option<i64>,
    pub validity: Option<String>,
    pub state: String,
    pub resolution: Option<String>,
    pub resolution_comment: Option<String>,
    pub resolved_by: Option<i64>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, sqlx::FromRow)]
pub struct Location {
    pub commit_sha: String,
    pub blob_sha: String,
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub start_column: i64,
    pub end_column: i64,
}

#[derive(Default)]
pub struct Filter {
    pub number: Option<i64>,
    pub state: Option<String>,
    pub secret_type: Option<String>,
    pub exclude_secret_types: Option<String>,
    pub resolution: Option<String>,
    pub validity: Option<String>,
    pub assignee: Option<String>,
    pub is_bypassed: Option<bool>,
    pub ascending: bool,
    pub updated: bool,
    pub offset: i64,
    pub limit: i64,
}

native_operation!(super::postgres::secret_scanning::org_alerts, super::sqlite::secret_scanning::org_alerts,
    fn org_alerts(pool: &DbPool, actor: i64, org: i64, filter: &Filter) -> Result<Vec<Alert>>);

#[derive(Default)]
pub struct AlertUpdate<'a> {
    pub state: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub comment: Option<&'a str>,
    pub assignee: Option<Option<&'a str>>,
    pub validity: Option<Option<&'a str>>,
}

pub struct Finding<'a> {
    pub hash: &'a str,
    pub secret_type: &'a str,
    pub display_name: &'a str,
    pub encrypted_secret: &'a [u8],
    pub nonce: &'a [u8],
    pub location: &'a Location,
}

impl Finding<'_> {
    /// The custom pattern that found this secret. Custom patterns name their findings
    /// `custom-pattern-<id>`.
    pub fn custom_pattern(&self) -> Option<i64> {
        self.secret_type
            .strip_prefix("custom-pattern-")?
            .parse()
            .ok()
    }
}

#[derive(Clone, sqlx::FromRow)]
pub struct Bypass {
    pub id: String,
    pub repo_id: i64,
    pub actor_id: i64,
    pub secret_hash: String,
    pub token_type: String,
    pub reason: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
}

native_operation!(super::postgres::secret_scanning::protection, super::sqlite::secret_scanning::protection,
    fn protection(pool: &DbPool, actor: i64, repo: i64, hash: &str, kind: &str) -> Result<Option<Bypass>>);
native_operation!(super::postgres::secret_scanning::bypass, super::sqlite::secret_scanning::bypass,
    fn bypass(pool: &DbPool, actor: i64, repo: i64, id: &str, reason: Option<&str>) -> Result<Bypass>);

native_operation!(super::postgres::secret_scanning::use_bypass,super::sqlite::secret_scanning::use_bypass,
    fn use_bypass(pool:&DbPool,repo:i64,actor:i64,hash:&str,accepted:DateTime<Utc>)->Result<()>);
native_operation!(super::postgres::secret_scanning::alert_bypasses,super::sqlite::secret_scanning::alert_bypasses,
    fn alert_bypasses(pool:&DbPool,keys:&[(i64,String)])->Result<Vec<Bypass>>);

#[derive(Clone, sqlx::FromRow)]
pub struct ScanJob {
    pub id: i64,
    pub repo_id: i64,
    pub head_sha: Option<String>,
    pub actor_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub scan_type: String,
    pub pattern_name: Option<String>,
    pub pattern_scope: Option<String>,
    pub status: String,
    pub attempt: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

native_operation!(super::postgres::secret_scanning::settings, super::sqlite::secret_scanning::settings,
    fn settings(pool: &DbPool, repo: i64) -> Result<Settings>);
native_operation!(super::postgres::secret_scanning::save_settings, super::sqlite::secret_scanning::save_settings,
    fn save_settings(pool: &DbPool, actor: i64, repo: i64, settings: &Settings) -> Result<()>);
native_operation!(super::postgres::secret_scanning::alerts, super::sqlite::secret_scanning::alerts,
    fn alerts(pool: &DbPool, actor: i64, repo: i64, filter: &Filter) -> Result<Vec<Alert>>);
native_operation!(super::postgres::secret_scanning::locations, super::sqlite::secret_scanning::locations,
    fn locations(pool: &DbPool, actor: i64, repo: i64, number: i64, offset: i64, limit: i64) -> Result<Vec<Location>>);
native_operation!(super::postgres::secret_scanning::update_alert, super::sqlite::secret_scanning::update_alert,
    fn update_alert(pool: &DbPool, actor: i64, repo: i64, number: i64, input: &AlertUpdate<'_>) -> Result<Alert>);
native_operation!(super::postgres::secret_scanning::record, super::sqlite::secret_scanning::record,
    fn record(pool: &DbPool, repo: i64, input: &Finding<'_>) -> Result<Option<(Alert, bool, bool)>>);
native_operation!(super::postgres::secret_scanning::enqueue, super::sqlite::secret_scanning::enqueue,
    fn enqueue(pool: &DbPool, repo: i64, head: &str, actor: i64) -> Result<()>);
native_operation!(super::postgres::secret_scanning::claim, super::sqlite::secret_scanning::claim,
    fn claim(pool: &DbPool) -> Result<Option<ScanJob>>);
#[derive(Clone, Copy)]
pub enum ScanCompletion { Completed, Failed, Retry }
impl ScanCompletion {
    pub(crate) fn status(&self) -> &'static str { match self { Self::Completed => "completed", Self::Failed => "failed", Self::Retry => "queued" } }
}

native_operation!(super::postgres::secret_scanning::finish, super::sqlite::secret_scanning::finish,
    fn finish(pool: &DbPool, id: i64, attempt: i64, completion: ScanCompletion) -> Result<()>);
native_operation!(super::postgres::secret_scanning::history, super::sqlite::secret_scanning::history,
    fn history(pool: &DbPool, actor: i64, repo: i64) -> Result<Vec<ScanJob>>);
native_operation!(super::postgres::secret_scanning::scanned, super::sqlite::secret_scanning::scanned,
    fn scanned(pool: &DbPool, repo: i64, commit: &str) -> Result<bool>);
// Keeps a claim alive and records `scanned` as scanned. False once another worker took the job over.
native_operation!(super::postgres::secret_scanning::heartbeat, super::sqlite::secret_scanning::heartbeat,
    fn heartbeat(pool: &DbPool, repo: i64, job: i64, attempt: i64, scanned: Option<&str>) -> Result<bool>);

/// SHA-256 of a location's path, the key that stands in for the path in the uniqueness index.
pub(super) fn path_digest(path: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(path.as_bytes()))
}

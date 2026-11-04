//! Delegated security-update work and receipts over the shared Dependabot alerts.
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use codeyard_db_core::models::Repository;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_FINDINGS: usize = 200;
pub const MAX_PACKAGES: usize = 2000;
pub const MAX_ALERTS: usize = 2000;
pub const LEASE_SECONDS: i64 = 180;

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Dependency security administration denied")]
    Denied,
    #[error("Dependency alert not found")]
    Missing,
    #[error("The dependency scan or repository changed; retry")]
    Stale,
    #[error("Enable dependency alerts under current administration first")]
    AlertsDisabled,
    #[error("Dependency snapshot limit reached; manual review is required")]
    Limit,
    #[error("{0}")]
    Invalid(&'static str),
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Feature {
    Alerts,
    Updates,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct State {
    pub repo_id: i64,
    pub alerts_enabled: bool,
    pub updates_enabled: bool,
    pub alerts_actor_id: Option<i64>,
    pub updates_actor_id: Option<i64>,
    pub owner_kind: String,
    pub owner_id: i64,
    pub private_consent: bool,
    pub generation: i64,
    pub next_scan_at: Option<DateTime<Utc>>,
    pub lease_nonce: Option<String>,
    pub lease_until: Option<DateTime<Utc>>,
    pub attempts: i32,
    pub last_status: String,
    pub last_error: Option<String>,
    pub last_scan_sha: Option<String>,
    pub last_scan_at: Option<DateTime<Utc>>,
    pub last_attempt_at: Option<DateTime<Utc>>,
}
impl State {
    pub fn same_owner(&self, repo: &Repository) -> bool {
        self.owner_kind == repo.owner_kind
            && Some(self.owner_id) == repo.owner_user_id.or(repo.owner_org_id)
    }
    pub fn scanned_in_lease(&self, sha: &str) -> bool {
        self.last_scan_sha.as_deref() == Some(sha)
            && self
                .last_scan_at
                .zip(self.last_attempt_at)
                .is_some_and(|(scan, attempt)| scan >= attempt)
    }
    pub fn actor(&self, feature: Feature) -> Option<i64> {
        match feature {
            Feature::Alerts => self.alerts_actor_id,
            Feature::Updates => self.updates_actor_id,
        }
    }
}
pub struct PageSnapshot {
    pub snapshot: Snapshot,
    pub state: Option<State>,
    pub proposals: BTreeMap<i64, (i32, String, bool)>,
}

pub struct Policy {
    pub alerts: bool,
    pub updates: bool,
    pub alerts_actor: Option<i64>,
    pub updates_actor: Option<i64>,
    pub owner_kind: String,
    pub owner_id: i64,
    pub consent: bool,
}
impl Policy {
    pub fn change(
        current: Option<&State>,
        repo: &Repository,
        actor: i64,
        feature: Feature,
        enabled: bool,
        private_consent: bool,
    ) -> Result<Option<Self>> {
        ensure!(
            !enabled || !repo.is_private || private_consent,
            Refusal::Invalid("Explicit private dependency export consent is required")
        );
        if current.is_none() && !enabled {
            return Ok(None);
        }
        let same = current.is_some_and(|s| s.same_owner(repo));
        let mut policy = Self {
            alerts: current.is_some_and(|s| same && s.alerts_enabled),
            updates: current.is_some_and(|s| same && s.updates_enabled),
            alerts_actor: current.and_then(|s| s.alerts_actor_id).filter(|_| same),
            updates_actor: current.and_then(|s| s.updates_actor_id).filter(|_| same),
            owner_kind: repo.owner_kind.clone(),
            owner_id: match repo.owner_kind.as_str() {
                "user" => repo.owner_user_id,
                "org" => repo.owner_org_id,
                _ => None,
            }
            .filter(|id| *id > 0)
            .ok_or(Refusal::Denied)?,
            consent: current.is_some_and(|s| same && s.private_consent),
        };
        match feature {
            Feature::Alerts => {
                policy.alerts = enabled;
                policy.alerts_actor = enabled.then_some(actor);
                policy.consent = enabled && repo.is_private;
                if !enabled {
                    policy.updates = false;
                    policy.updates_actor = None;
                }
            }
            Feature::Updates => {
                if enabled {
                    ensure!(
                        policy.alerts
                            && policy.alerts_actor.is_some()
                            && (!repo.is_private || policy.consent),
                        Refusal::AlertsDisabled
                    );
                }
                policy.updates = enabled;
                policy.updates_actor = enabled.then_some(actor);
            }
        }
        Ok(Some(policy))
    }
}
pub const COLUMNS:&str="repo_id,alerts_enabled,updates_enabled,alerts_actor_id,updates_actor_id,owner_kind,owner_id,private_consent,generation,next_scan_at,lease_nonce,lease_until,attempts,last_status,last_error,last_scan_sha,last_scan_at,last_attempt_at";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub repo_id: i64,
    pub generation: i64,
    pub nonce: String,
    pub actor_id: i64,
    pub updates_actor_id: Option<i64>,
}
impl Job {
    pub fn matches(&self, state: &State) -> bool {
        self.repo_id == state.repo_id
            && self.generation == state.generation
            && state.lease_nonce.as_deref() == Some(self.nonce.as_str())
            && state.alerts_enabled
            && state.alerts_actor_id == Some(self.actor_id)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub path: String,
    pub name: String,
    pub version: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Finding {
    pub package: InstalledPackage,
    pub advisory_id: String,
    pub summary: String,
    pub details: String,
    pub severity: String,
    pub url: String,
    pub fixed_versions: Vec<String>,
}
fn bounded(text: &str, limit: usize) -> bool {
    text.len() <= limit && !text.contains('\0')
}
pub fn sha(sha: &str) -> bool {
    codeyard_support::git_ids::is_full_commit_id(sha)
        && !sha.bytes().any(|byte| byte.is_ascii_uppercase())
}
impl InstalledPackage {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.path.is_empty()
                && bounded(&self.path, 2048)
                && !self.path.chars().any(char::is_control)
                && !self.name.is_empty()
                && bounded(&self.name, 214)
                && self.name.is_ascii()
                && !self.name.chars().any(char::is_control)
                && !self.version.is_empty()
                && bounded(&self.version, 256)
                && !self.version.chars().any(char::is_control),
            Refusal::Invalid("Invalid installed dependency")
        );
        Ok(())
    }
}
impl Finding {
    pub fn key(&self) -> String {
        let mut digest = Sha256::new();
        for part in [&self.package.path, &self.package.name, &self.advisory_id] {
            digest.update(part.as_bytes());
            digest.update([0]);
        }
        hex::encode(digest.finalize())
    }
    pub fn validate(&self) -> Result<()> {
        self.package.validate()?;
        ensure!(
            self.advisory_id
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
                && self.advisory_id.len() <= 128
                && self
                    .advisory_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
            Refusal::Invalid("Invalid advisory identifier")
        );
        ensure!(
            !self.summary.is_empty()
                && bounded(&self.summary, 1024)
                && bounded(&self.details, 32768)
                && self.url == format!("https://osv.dev/vulnerability/{}", self.advisory_id)
                && matches!(
                    self.severity.as_str(),
                    "UNKNOWN" | "LOW" | "MODERATE" | "HIGH" | "CRITICAL"
                )
                && self.fixed_versions.len() <= 16
                && self
                    .fixed_versions
                    .iter()
                    .all(|v| bounded(v, 256) && semver::Version::parse(v).is_ok()),
            Refusal::Invalid("Invalid dependency advisory")
        );
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AlertState {
    Open,
    Dismissed,
    Resolved,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Update {
    pub branch: String,
    pub base_sha: String,
    pub commit_sha: String,
    pub old_version: String,
    pub fixed_version: String,
    pub requested_by: i64,
    pub pull_request_id: Option<i64>,
}
impl Update {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.branch.starts_with("codeyard/security/")
                && self.branch.len() <= 255
                && codeyard_git::branch_ref(&self.branch).is_some()
                && sha(&self.base_sha)
                && sha(&self.commit_sha)
                && bounded(&self.old_version, 256)
                && bounded(&self.fixed_version, 256)
                && self.requested_by > 0
                && self.pull_request_id.is_none_or(|id| id > 0),
            Refusal::Invalid("Invalid dependency update")
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Publication {
    pub job: Job,
    pub key: String,
    pub update: Update,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Alert {
    pub finding: Finding,
    pub state: AlertState,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub seen_sha: String,
    pub dismissal: Option<String>,
    pub update: Option<Update>,
    pub manual_reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub version: u8,
    pub packages: Vec<InstalledPackage>,
    pub unsupported: usize,
    pub alerts: BTreeMap<String, Alert>,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            version: 1,
            packages: vec![],
            unsupported: 0,
            alerts: BTreeMap::new(),
        }
    }
}
impl Snapshot {
    pub fn decode(raw: &str) -> Result<Self> {
        ensure!(raw.len() <= MAX_SNAPSHOT_BYTES, Refusal::Limit);
        let snapshot: Self = serde_json::from_str(raw)?;
        snapshot.validate()?;
        Ok(snapshot)
    }
    pub fn encode(&self) -> Result<String> {
        self.validate()?;
        let raw = serde_json::to_string(self)?;
        ensure!(raw.len() <= MAX_SNAPSHOT_BYTES, Refusal::Limit);
        Ok(raw)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            Refusal::Invalid("Dependency snapshot requires a newer server")
        );
        ensure!(
            self.packages.len() <= MAX_PACKAGES
                && self.alerts.len() <= MAX_ALERTS
                && self.unsupported <= MAX_PACKAGES,
            Refusal::Limit
        );
        for package in &self.packages {
            package.validate()?;
        }
        for (key, alert) in &self.alerts {
            alert.finding.validate()?;
            ensure!(
                key == &alert.finding.key()
                    && sha(&alert.seen_sha)
                    && alert.dismissal.as_ref().is_none_or(|s| bounded(s, 1024))
                    && alert
                        .manual_reason
                        .as_ref()
                        .is_none_or(|s| bounded(s, 1024)),
                Refusal::Invalid("Invalid stored dependency alert")
            );
            if let Some(update) = &alert.update {
                update.validate()?;
            }
        }
        Ok(())
    }
    pub fn manual(&mut self, key: &str, version: &str, reason: &str) -> Result<()> {
        ensure!(
            !reason.trim().is_empty() && bounded(reason, 1024),
            Refusal::Invalid("Invalid manual update reason")
        );
        let alert = self.alerts.get_mut(key).ok_or(Refusal::Missing)?;
        ensure!(
            alert.state == AlertState::Open && alert.finding.package.version == version,
            Refusal::Stale
        );
        alert.manual_reason = Some(reason.into());
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    version: String,
    fixed_versions: Vec<String>,
    update: Option<Update>,
    manual_reason: Option<String>,
}

/// Persist proposal receipts, never a second inventory or alert lifecycle.
pub(crate) fn receipts(snapshot: &Snapshot) -> Result<String> {
    let receipts: BTreeMap<_, _> = snapshot
        .alerts
        .iter()
        .filter(|(_, alert)| alert.update.is_some() || alert.manual_reason.is_some())
        .map(|(key, alert)| {
            (
                key,
                Receipt {
                    version: alert.finding.package.version.clone(),
                    fixed_versions: alert.finding.fixed_versions.clone(),
                    update: alert.update.clone(),
                    manual_reason: alert.manual_reason.clone(),
                },
            )
        })
        .collect();
    let raw = serde_json::to_string(&receipts)?;
    ensure!(raw.len() <= MAX_SNAPSHOT_BYTES, Refusal::Limit);
    Ok(raw)
}

pub(crate) fn project(
    rows: Vec<crate::backend::db::dependabot_alerts::Alert>,
    inventory: Vec<crate::backend::dependency_graph::Snapshot>,
    raw: &str,
) -> Result<Snapshot> {
    use serde_json::Value;
    let receipts: BTreeMap<String, Receipt> = serde_json::from_str(raw)?;
    let mut snapshot = Snapshot::default();
    let sha = inventory
        .iter()
        .find(|snapshot| {
            snapshot.detector.name == "Codeyard" && snapshot.job.correlator == "lockfiles"
        })
        .or_else(|| inventory.first())
        .map(|snapshot| snapshot.sha.clone())
        .unwrap_or_else(|| "0".repeat(40));
    snapshot.packages = crate::backend::dependency_graph::packages(&inventory)
        .into_iter()
        .map(|package| InstalledPackage {
            path: package.manifest,
            name: package.name,
            version: package.version,
        })
        .collect();
    for row in rows {
        let advisory: Value = serde_json::from_str(&row.advisory)?;
        let vulnerability: Value = serde_json::from_str(&row.vulnerability)?;
        let package = InstalledPackage {
            path: if row.ecosystem == "npm" && row.manifest == "package-lock.json" {
                format!("node_modules/{}", row.package)
            } else {
                row.manifest
            },
            name: row.package,
            version: row.version,
        };
        let fixed_versions = vulnerability["first_patched_version"]
            .as_str()
            .and_then(|v| semver::Version::parse(v).ok())
            .map(|v| vec![v.to_string()])
            .unwrap_or_default();
        let finding = Finding {
            package,
            advisory_id: row.ghsa_id.clone(),
            summary: advisory["summary"].as_str().unwrap_or(&row.ghsa_id).into(),
            details: advisory["description"].as_str().unwrap_or("").into(),
            severity: match advisory["severity"].as_str().unwrap_or("") {
                "low" => "LOW",
                "medium" | "moderate" => "MODERATE",
                "high" => "HIGH",
                "critical" => "CRITICAL",
                _ => "UNKNOWN",
            }
            .into(),
            url: format!("https://osv.dev/vulnerability/{}", row.ghsa_id),
            fixed_versions,
        };
        let key = finding.key();
        let receipt = receipts
            .get(&key)
            .filter(|r| r.version == finding.package.version);
        let manual_reason = receipt
            .filter(|r| r.fixed_versions == finding.fixed_versions)
            .and_then(|r| r.manual_reason.clone());
        snapshot.alerts.insert(
            key,
            Alert {
                finding,
                state: match row.state.as_str() {
                    "open" => AlertState::Open,
                    "dismissed" => AlertState::Dismissed,
                    _ => AlertState::Resolved,
                },
                first_seen: row.created_at,
                last_seen: row.updated_at,
                seen_sha: sha.clone(),
                dismissal: row.dismissed_comment.or(row.dismissed_reason),
                update: receipt.and_then(|r| r.update.clone()),
                manual_reason,
            },
        );
    }
    Ok(snapshot)
}
native_operation!(crate::backend::db::postgres::repositories::dependencies::page_snapshot,crate::backend::db::sqlite::repositories::dependencies::page_snapshot,fn page_snapshot(pool:&DbPool,actor:impl Into<super::super::credentials::Actor>,repo:i64)->Result<PageSnapshot>);
native_operation!(crate::backend::db::postgres::repositories::dependencies::state,crate::backend::db::sqlite::repositories::dependencies::state,fn state(pool:&DbPool,repo:i64)->Result<Option<State>>);
native_operation!(crate::backend::db::postgres::repositories::dependencies::configure,crate::backend::db::sqlite::repositories::dependencies::configure,fn configure(pool:&DbPool,actor:impl Into<super::super::credentials::Actor>,repo:i64,feature:Feature,enabled:bool,private_consent:bool)->Result<()>);
native_operation!(crate::backend::db::postgres::repositories::dependencies::enqueue,crate::backend::db::sqlite::repositories::dependencies::enqueue,fn enqueue(pool:&DbPool,repo:i64)->Result<()>);
native_operation!(crate::backend::db::postgres::repositories::dependencies::claim,crate::backend::db::sqlite::repositories::dependencies::claim,fn claim(pool:&DbPool,nonce:&str)->Result<Option<Job>>);
native_operation!(crate::backend::db::postgres::repositories::dependencies::check_job,crate::backend::db::sqlite::repositories::dependencies::check_job,fn check_job(pool:&DbPool,job:&Job,feature:Feature)->Result<()>);
native_operation!(crate::backend::db::postgres::repositories::dependencies::snapshot,crate::backend::db::sqlite::repositories::dependencies::snapshot,fn snapshot(pool:&DbPool,actor:i64,repo:i64)->Result<Snapshot>);
native_operation!(crate::backend::db::postgres::repositories::dependencies::complete_scan,crate::backend::db::sqlite::repositories::dependencies::complete_scan,fn complete_scan(pool:&DbPool,job:&Job,sha:&str)->Result<()>);
native_operation!(crate::backend::db::postgres::repositories::dependencies::release,crate::backend::db::sqlite::repositories::dependencies::release,fn release(pool:&DbPool,job:&Job,status:Option<&str>,error:Option<&str>)->Result<()>);
native_operation!(crate::backend::db::postgres::repositories::dependencies::plan_update,crate::backend::db::sqlite::repositories::dependencies::plan_update,fn plan_update(pool:&DbPool,job:&Job,key:&str,update:&Update)->Result<()>);
native_operation!(crate::backend::db::postgres::repositories::dependencies::attach_pull,crate::backend::db::sqlite::repositories::dependencies::attach_pull,fn attach_pull(pool:&DbPool,actor:i64,repo:i64,key:&str,commit:&str,pull:i64)->Result<()>);
native_operation!(crate::backend::db::postgres::repositories::dependencies::manual,crate::backend::db::sqlite::repositories::dependencies::manual,fn manual(pool:&DbPool,job:&Job,key:&str,version:&str,reason:&str)->Result<()>);
native_operation!(crate::backend::db::postgres::repositories::dependencies::proposal_number,crate::backend::db::sqlite::repositories::dependencies::proposal_number,fn proposal_number(pool:&DbPool,actor:i64,repo:i64,branch:&str)->Result<Option<i32>>);
#[cfg(test)]
mod tests;

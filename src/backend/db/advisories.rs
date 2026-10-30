//! Confidential repository advisories. Publication is explicit; CVE requests never mint an ID.
use super::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Advisory administration denied")]
    Denied,
    #[error("Advisory not found")]
    Missing,
    #[error("Invalid advisory")]
    Invalid,
    #[error("The advisory state does not allow this operation")]
    Conflict,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub ecosystem: String,
    pub name: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vulnerability {
    pub package: Package,
    pub vulnerable_version_range: Option<String>,
    pub patched_versions: Option<String>,
    pub vulnerable_functions: Option<Vec<String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credit {
    pub login: String,
    #[serde(rename = "type")]
    pub kind: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Data {
    pub summary: String,
    pub description: String,
    pub cve_id: Option<String>,
    #[serde(default)]
    pub vulnerabilities: Vec<Vulnerability>,
    pub cwe_ids: Option<Vec<String>>,
    pub credits: Option<Vec<Credit>>,
    pub severity: Option<String>,
    pub cvss_vector_string: Option<String>,
}
impl Data {
    pub fn parse(value: &Value, previous: Option<&Self>, report: bool) -> Result<Self> {
        let mut map = value.as_object().ok_or(Refusal::Invalid)?.clone();
        for control in [
            "state",
            "start_private_fork",
            "collaborating_users",
            "collaborating_teams",
        ] {
            map.remove(control);
        }
        if previous.is_none() && !report {
            anyhow::ensure!(map.contains_key("vulnerabilities"), Refusal::Invalid);
        }
        let mut full = if let Some(previous) = previous {
            serde_json::to_value(previous)?.as_object().unwrap().clone()
        } else {
            serde_json::Map::new()
        };
        if map.get("severity").is_some_and(|v| !v.is_null()) {
            anyhow::ensure!(
                map.get("cvss_vector_string").is_none_or(Value::is_null)
                    && (previous.is_none_or(|data| data.cvss_vector_string.is_none())
                        || map.get("cvss_vector_string").is_some_and(Value::is_null)),
                Refusal::Invalid
            );
            full.insert("cvss_vector_string".into(), Value::Null);
        }
        full.extend(map);
        let mut data: Self =
            serde_json::from_value(Value::Object(full)).map_err(|_| Refusal::Invalid)?;
        super::repositories::security::NewReport::new(&data.summary, &data.description)
            .map_err(|_| Refusal::Invalid)?;
        anyhow::ensure!(
            data.severity
                .as_deref()
                .is_none_or(|v| matches!(v, "low" | "medium" | "high" | "critical")),
            Refusal::Invalid
        );
        if let Some(vector) = &data.cvss_vector_string {
            let score = cvss_score(vector)?;
            data.severity = Some(
                if score >= 9.0 {
                    "critical"
                } else if score >= 7.0 {
                    "high"
                } else if score >= 4.0 {
                    "medium"
                } else {
                    "low"
                }
                .into(),
            );
        }
        if let Some(cve) = &data.cve_id {
            let parts = cve.split('-').collect::<Vec<_>>();
            anyhow::ensure!(
                parts.len() == 3
                    && parts[0] == "CVE"
                    && parts[1].len() == 4
                    && parts[1].bytes().all(|b| b.is_ascii_digit())
                    && (4..=16).contains(&parts[2].len())
                    && parts[2].bytes().all(|b| b.is_ascii_digit()),
                Refusal::Invalid
            );
        }
        anyhow::ensure!(data.vulnerabilities.len() <= 100, Refusal::Invalid);
        for vulnerability in &data.vulnerabilities {
            anyhow::ensure!(
                matches!(
                    vulnerability.package.ecosystem.as_str(),
                    "rubygems"
                        | "npm"
                        | "pip"
                        | "maven"
                        | "nuget"
                        | "composer"
                        | "go"
                        | "rust"
                        | "erlang"
                        | "actions"
                        | "pub"
                        | "other"
                        | "swift"
                ) && vulnerability
                    .package
                    .name
                    .as_ref()
                    .is_none_or(|n| !n.is_empty()
                        && n.len() <= 255
                        && !n.chars().any(char::is_control)),
                Refusal::Invalid
            );
            anyhow::ensure!(
                vulnerability
                    .vulnerable_version_range
                    .as_ref()
                    .is_none_or(|v| v.len() <= 1024)
                    && vulnerability
                        .patched_versions
                        .as_ref()
                        .is_none_or(|v| v.len() <= 1024)
                    && vulnerability
                        .vulnerable_functions
                        .as_ref()
                        .is_none_or(|v| v.len() <= 100 && v.iter().all(|f| f.len() <= 1024)),
                Refusal::Invalid
            );
        }
        if let Some(cwes) = &data.cwe_ids {
            anyhow::ensure!(
                cwes.len() <= 64
                    && cwes
                        .iter()
                        .all(|s| s.strip_prefix("CWE-").is_some_and(|id| !id.is_empty()
                            && id.len() <= 8
                            && id.bytes().all(|b| b.is_ascii_digit()))),
                Refusal::Invalid
            );
        }
        if let Some(credits) = &data.credits {
            anyhow::ensure!(
                credits.len() <= 100
                    && credits.iter().all(|c| !c.login.is_empty()
                        && c.login.len() <= 255
                        && matches!(
                            c.kind.as_str(),
                            "finder"
                                | "reporter"
                                | "analyst"
                                | "coordinator"
                                | "remediation_developer"
                                | "remediation_reviewer"
                                | "remediation_verifier"
                                | "tool"
                                | "sponsor"
                                | "other"
                        )),
                Refusal::Invalid
            );
        }
        for name in ["collaborating_users", "collaborating_teams"] {
            if let Some(value) = value.get(name) {
                anyhow::ensure!(
                    value.is_null() || value.as_array().is_some_and(Vec::is_empty),
                    Refusal::Invalid
                );
            }
        }
        Ok(data)
    }
    pub fn from_row(row: &Advisory) -> Result<Self> {
        Ok(serde_json::from_str(&row.document)?)
    }
}
/// CVSS 3.0/3.1 base vectors. Unsupported metric sets are rejected rather than guessed.
pub fn cvss_score(vector: &str) -> Result<f64> {
    let (version, metrics) = vector.split_once('/').ok_or(Refusal::Invalid)?;
    anyhow::ensure!(matches!(version, "CVSS:3.0" | "CVSS:3.1"), Refusal::Invalid);
    let mut fields = std::collections::BTreeMap::new();
    for metric in metrics.split('/') {
        let (key, value) = metric.split_once(':').ok_or(Refusal::Invalid)?;
        anyhow::ensure!(fields.insert(key, value).is_none(), Refusal::Invalid);
    }
    anyhow::ensure!(fields.len() == 8, Refusal::Invalid);
    let changed = match fields.get("S").copied() {
        Some("U") => false,
        Some("C") => true,
        _ => anyhow::bail!(Refusal::Invalid),
    };
    let metric = |name: &str, weights: &[(&str, f64)]| -> Result<f64> {
        let value = fields.get(name).ok_or(Refusal::Invalid)?;
        weights
            .iter()
            .find(|(key, _)| key == value)
            .map(|(_, score)| *score)
            .ok_or_else(|| Refusal::Invalid.into())
    };
    let av = metric("AV", &[("N", 0.85), ("A", 0.62), ("L", 0.55), ("P", 0.2)])?;
    let ac = metric("AC", &[("L", 0.77), ("H", 0.44)])?;
    let pr = metric(
        "PR",
        &[
            ("N", 0.85),
            ("L", if changed { 0.68 } else { 0.62 }),
            ("H", if changed { 0.5 } else { 0.27 }),
        ],
    )?;
    let ui = metric("UI", &[("N", 0.85), ("R", 0.62)])?;
    let weights = [("H", 0.56), ("L", 0.22), ("N", 0.0)];
    let isc = 1.0
        - (1.0 - metric("C", &weights)?)
            * (1.0 - metric("I", &weights)?)
            * (1.0 - metric("A", &weights)?);
    let impact = if changed {
        7.52 * (isc - 0.029) - 3.25 * (isc - 0.02).powi(15)
    } else {
        6.42 * isc
    };
    if impact <= 0.0 {
        return Ok(0.0);
    }
    let base = ((impact + 8.22 * av * ac * pr * ui) * if changed { 1.08 } else { 1.0 }).min(10.0);
    Ok((base * 10.0 - 1e-10).ceil() / 10.0)
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Advisory {
    pub id: i64,
    pub ghsa_id: String,
    pub repo_id: i64,
    pub author_id: Option<i64>,
    pub state: String,
    pub document: String,
    pub report_id: Option<i64>,
    pub report_accepted: bool,
    pub private_fork_id: Option<i64>,
    pub cve_requested_at: Option<DateTime<Utc>>,
    pub publisher_id: Option<i64>,
    pub published_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Clone, Copy)]
pub enum Scope {
    Repository(i64),
    Organization(i64),
}
#[derive(Clone, Copy, Default)]
pub enum Sort {
    #[default]
    Created,
    Updated,
    Published,
}
impl Sort {
    pub fn timestamp(&self, row: &Advisory) -> DateTime<Utc> {
        match self {
            Self::Created => row.created_at,
            Self::Updated => row.updated_at,
            Self::Published => row.published_at.unwrap_or(row.created_at),
        }
    }
    pub(crate) fn column(&self) -> &'static str {
        match self {
            Self::Created => "a.created_at",
            Self::Updated => "a.updated_at",
            Self::Published => "COALESCE(a.published_at,a.created_at)",
        }
    }
}
#[derive(Default)]
pub struct Listing<'a> {
    pub state: Option<&'a str>,
    pub sort: Sort,
    pub ascending: bool,
    pub before: bool,
    pub cursor: Option<(DateTime<Utc>, i64)>,
    pub offset: i64,
    pub limit: i64,
}
pub fn identifier() -> String {
    const LETTERS: &[u8] = b"23456789cfghjmpqrvwx";
    let random = uuid::Uuid::new_v4();
    let mut value = String::from("GHSA");
    for group in random.as_bytes()[..12].chunks(4) {
        value.push('-');
        for byte in group {
            value.push(LETTERS[*byte as usize % LETTERS.len()] as char);
        }
    }
    value
}

/// A UI/preflight projection, never a mutation permit.
pub(crate) fn administration_sql(level: &str) -> String {
    let manager = super::organizations::roles::security_manager_condition("u.id", "r.owner_org_id");
    format!("SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=$1 WHERE r.id=$2 AND NOT r.is_gist AND NOT r.is_archived AND NOT u.is_banned AND NOT u.must_change_password AND NOT EXISTS(SELECT 1 FROM repository_migration_locks l WHERE l.repo_id=r.id) AND ((r.owner_kind='user' AND (r.owner_user_id=u.id OR u.is_admin)) OR ({level})>=5 OR (r.owner_kind='org' AND {manager})))")
}

native_operation!(crate::backend::db::postgres::advisories::can_administer,crate::backend::db::sqlite::advisories::can_administer,
    fn can_administer(pool:&DbPool,actor:i64,repo:i64)->Result<bool>);
native_operation!(crate::backend::db::postgres::advisories::list,crate::backend::db::sqlite::advisories::list,
    fn list(pool:&DbPool,actor:Option<i64>,scope:Scope,options:&Listing<'_>)->Result<Vec<Advisory>>);
native_operation!(crate::backend::db::postgres::advisories::get,crate::backend::db::sqlite::advisories::get,
    fn get(pool:&DbPool,actor:Option<i64>,repo:i64,ghsa:&str)->Result<Option<Advisory>>);
native_operation!(crate::backend::db::postgres::advisories::create,crate::backend::db::sqlite::advisories::create,
    fn create(pool:&DbPool,actor:i64,repo:i64,value:&Value,report:bool)->Result<Advisory>);
native_operation!(crate::backend::db::postgres::advisories::update,crate::backend::db::sqlite::advisories::update,
    fn update(pool:&DbPool,actor:i64,repo:i64,ghsa:&str,patch:&Value)->Result<Advisory>);
native_operation!(crate::backend::db::postgres::advisories::request_cve,crate::backend::db::sqlite::advisories::request_cve,
    fn request_cve(pool:&DbPool,actor:i64,repo:i64,ghsa:&str)->Result<()>);
native_operation!(crate::backend::db::postgres::advisories::attach_fork,crate::backend::db::sqlite::advisories::attach_fork,
    fn attach_fork(pool:&DbPool,actor:i64,repo:i64,ghsa:&str,fork:i64)->Result<()>);

#[cfg(test)]
mod tests {
    use super::cvss_score;
    #[test]
    fn cvss_base_vectors_are_scored_without_inventing_metrics() {
        assert_eq!(
            cvss_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H").unwrap(),
            9.8
        );
        assert_eq!(
            cvss_score("CVSS:3.0/AV:N/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H").unwrap(),
            10.0
        );
        assert_eq!(
            cvss_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:N").unwrap(),
            0.0
        );
        assert!(cvss_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/AV:N").is_err());
    }
}

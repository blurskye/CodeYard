//! Local advisories and snapshot matches. No advisory read contacts an external feed.
use super::{
    advisories::{self, Advisory, Data, Refusal},
    DbPool,
};
use crate::backend::dependency_graph::{self, Package, Snapshot};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Alert {
    pub repo_id: i64,
    pub number: i64,
    pub ghsa_id: String,
    pub manifest: String,
    pub ecosystem: String,
    pub package: String,
    pub version: String,
    pub scope: String,
    pub state: String,
    pub advisory: String,
    pub vulnerability: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub fixed_at: Option<DateTime<Utc>>,
    pub dismissed_at: Option<DateTime<Utc>>,
    pub dismissed_by: Option<i64>,
    pub dismissed_reason: Option<String>,
    pub dismissed_comment: Option<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub state: String,
    pub dismissed_reason: Option<String>,
    pub dismissed_comment: Option<String>,
}
impl Update {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.state.as_str(), "open" | "dismissed"),
            Refusal::Invalid
        );
        ensure!(
            self.dismissed_comment
                .as_ref()
                .is_none_or(|s| s.chars().count() <= 280),
            Refusal::Invalid
        );
        ensure!(
            if self.state == "dismissed" {
                self.dismissed_reason.as_deref().is_some_and(|s| {
                    matches!(
                        s,
                        "fix_started"
                            | "inaccurate"
                            | "no_bandwidth"
                            | "not_used"
                            | "tolerable_risk"
                    )
                })
            } else {
                self.dismissed_reason.is_none() && self.dismissed_comment.is_none()
            },
            Refusal::Invalid
        );
        Ok(())
    }
}

pub fn import_document(value: &Value) -> Result<Value> {
    validate_document(value, 100)
}

pub(crate) fn osv_document(value: &Value) -> Result<Value> {
    validate_document(value, crate::backend::vulnerabilities::MAX_AFFECTED_PACKAGES)
}

fn validate_document(value: &Value, max_packages: usize) -> Result<Value> {
    ensure!(
        serde_json::to_vec(value)?.len() <= 1024 * 1024,
        Refusal::Invalid
    );
    let mut value = value.as_object().ok_or(Refusal::Invalid)?.clone();
    let allowed = [
        "ghsa_id",
        "cve_id",
        "url",
        "html_url",
        "repository_advisory_url",
        "summary",
        "description",
        "type",
        "severity",
        "source_code_location",
        "identifiers",
        "references",
        "published_at",
        "updated_at",
        "github_reviewed_at",
        "nvd_published_at",
        "withdrawn_at",
        "vulnerabilities",
        "cvss",
        "cvss_severities",
        "epss",
        "cwes",
        "credits",
    ];
    ensure!(
        value.keys().all(|k| allowed.contains(&k.as_str())),
        Refusal::Invalid
    );
    let id = value
        .entry("ghsa_id")
        .or_insert_with(|| json!(advisories::identifier()))
        .as_str()
        .ok_or(Refusal::Invalid)?;
    ensure!(
        id.len() == 19
            && id.starts_with("GHSA-")
            && id[5..].split('-').count() == 3
            && id[5..]
                .split('-')
                .all(|s| s.len() == 4 && s.bytes().all(|b| b"23456789cfghjmpqrvwx".contains(&b))),
        Refusal::Invalid
    );
    let now = json!(Utc::now());
    for key in ["published_at", "updated_at"] {
        value.entry(key).or_insert_with(|| now.clone());
    }
    for key in [
        "cve_id",
        "description",
        "repository_advisory_url",
        "source_code_location",
        "github_reviewed_at",
        "nvd_published_at",
        "withdrawn_at",
        "credits",
        "cwes",
        "cvss",
    ] {
        value.entry(key).or_insert(Value::Null);
    }
    value.entry("type").or_insert(json!("reviewed"));
    value.entry("references").or_insert(json!([]));
    let id = value["ghsa_id"].as_str().unwrap().to_owned();
    value.insert(
        "url".into(),
        json!(format!(
            "{}/advisories/{id}",
            crate::backend::api::github::api_url("")
        )),
    );
    value.insert(
        "html_url".into(),
        json!(format!(
            "{}/api/v3/advisories/{id}",
            crate::backend::api::http::config::link_origin()
        )),
    );
    let mut identifiers = vec![json!({"type":"GHSA","value":id})];
    if let Some(cve) = value["cve_id"].as_str() {
        identifiers.push(json!({"type":"CVE","value":cve}));
    }
    value.insert("identifiers".into(), json!(identifiers));
    let summary = value
        .get("summary")
        .and_then(Value::as_str)
        .ok_or(Refusal::Invalid)?;
    ensure!(
        !summary.trim().is_empty() && summary.len() <= 1024,
        Refusal::Invalid
    );
    ensure!(
        value["description"].is_null()
            || value["description"]
                .as_str()
                .is_some_and(|s| s.len() <= 65535),
        Refusal::Invalid
    );
    ensure!(
        matches!(
            value["severity"].as_str(),
            Some("low" | "medium" | "high" | "critical" | "unknown")
        ),
        Refusal::Invalid
    );
    ensure!(
        matches!(
            value["type"].as_str(),
            Some("reviewed" | "unreviewed" | "malware")
        ),
        Refusal::Invalid
    );
    for key in ["published_at", "updated_at"] {
        ensure!(value[key].as_str().is_some(), Refusal::Invalid);
    }
    for key in ["references", "cwes", "credits"] {
        ensure!(
            value[key].is_null() || value[key].as_array().is_some_and(|a| a.len() <= 100),
            Refusal::Invalid
        );
    }
    ensure!(
        value["references"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|v| v.as_str().is_some_and(|s| url::Url::parse(s).is_ok())),
        Refusal::Invalid
    );
    ensure!(
        value["cwes"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|v| v["cwe_id"].as_str().is_some() && v["name"].as_str().is_some()),
        Refusal::Invalid
    );
    // Imported credits and scoring extensions are not used as local account or scoring authority.
    value.insert("credits".into(), Value::Null);
    value.remove("cvss_severities");
    value.remove("epss");
    if !value["cvss"].is_null() {
        let vector = value["cvss"]["vector_string"]
            .as_str()
            .ok_or(Refusal::Invalid)?;
        let score = advisories::cvss_score(vector)?;
        value.insert("cvss".into(), json!({"vector_string":vector,"score":score}));
    }
    for key in [
        "published_at",
        "updated_at",
        "github_reviewed_at",
        "nvd_published_at",
        "withdrawn_at",
    ] {
        ensure!(
            value[key].is_null()
                || value[key]
                    .as_str()
                    .is_some_and(|s| DateTime::parse_from_rfc3339(s).is_ok()),
            Refusal::Invalid
        );
    }
    let vulnerabilities = value
        .get("vulnerabilities")
        .and_then(Value::as_array)
        .ok_or(Refusal::Invalid)?;
    ensure!(vulnerabilities.len() <= max_packages, Refusal::Invalid);
    for v in vulnerabilities {
        ensure!(
            v["first_patched_version"].is_null() || v["first_patched_version"].is_string(),
            Refusal::Invalid
        );
        ensure!(
            v["vulnerable_functions"].is_null()
                || v["vulnerable_functions"]
                    .as_array()
                    .is_some_and(|a| a.len() <= 100 && a.iter().all(Value::is_string)),
            Refusal::Invalid
        );
        ensure!(
            v["package"]["name"]
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.len() <= 255 && !s.contains('\0'))
                && v["package"]["ecosystem"].as_str().is_some_and(|s| !s.contains('\0'))
                && v["vulnerable_version_range"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty() && s.len() <= 1024),
            Refusal::Invalid
        );
    }
    Ok(Value::Object(value))
}

pub(crate) fn published(row: Advisory) -> Result<Value> {
    let data = Data::from_row(&row)?;
    let vulnerabilities: Vec<_> = data.vulnerabilities.iter().filter(|v| v.package.name.is_some() && v.vulnerable_version_range.is_some()).map(|v| json!({"package":v.package,"vulnerable_version_range":v.vulnerable_version_range,"first_patched_version":v.patched_versions.as_ref().map(|s|json!(s)),"vulnerable_functions":v.vulnerable_functions})).collect();
    let cvss = data
        .cvss_vector_string
        .as_ref()
        .map(|v| json!({"vector_string":v,"score":advisories::cvss_score(v).ok()}));
    import_document(
        &json!({"ghsa_id":row.ghsa_id,"summary":data.summary,"description":data.description,"cve_id":data.cve_id,"severity":data.severity.as_deref().unwrap_or("unknown"),"published_at":row.published_at,"updated_at":row.updated_at,"vulnerabilities":vulnerabilities,"cvss":cvss}),
    )
}

/// The OSV (ecosystem, name) pairs an inventory can match, written the way `osv_packages`
/// stores them, as a JSON array. Matching loads only the OSV records these pairs select.
pub(crate) fn osv_keys(snapshots: &[Snapshot]) -> String {
    let keys: std::collections::BTreeSet<(&str, String)> = dependency_graph::packages(snapshots)
        .iter()
        .filter_map(|package| {
            let ecosystem =
                crate::backend::vulnerabilities::ecosystem(ecosystem(&package.ecosystem))?;
            Some((
                ecosystem,
                crate::backend::vulnerabilities::name(ecosystem, &package.name),
            ))
        })
        .collect();
    json!(keys).to_string()
}

fn ecosystem(purl: &str) -> &str {
    match purl {
        "cargo" => "rust",
        "pypi" => "pip",
        "gem" => "rubygems",
        "golang" => "go",
        _ => purl,
    }
}
/// Only concrete SemVer versions are compared; unknown version schemes are not guessed.
pub fn affected(version: &str, range: &str) -> bool {
    let Ok(version) = semver::Version::parse(version.strip_prefix('v').unwrap_or(version)) else {
        return false;
    };
    range.split("||").any(|part| {
        let part = part.trim();
        if part.is_empty() {
            return false;
        }
        let range = if part.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
            format!("={part}")
        } else {
            part.to_owned()
        };
        semver::VersionReq::parse(&range).is_ok_and(|r| r.matches(&version))
    })
}
pub(crate) fn osv_records(rows: Vec<String>) -> Result<(std::collections::BTreeMap<String, crate::backend::vulnerabilities::Advisory>, Vec<String>)> {
    let mut records = std::collections::BTreeMap::new();
    let mut aliases = std::collections::BTreeSet::new();
    for data in rows {
        let record: crate::backend::vulnerabilities::Advisory = serde_json::from_str(&data)?;
        aliases.insert(record.global_document()?["ghsa_id"].as_str().unwrap().to_owned());
        records.insert(record.id.clone(), record);
    }
    Ok((records, aliases.into_iter().collect()))
}

pub(crate) fn catalogue_for_records(
    catalogue: &mut Vec<Value>,
    records: &mut std::collections::BTreeMap<String, crate::backend::vulnerabilities::Advisory>,
    overrides: Vec<(String, Option<String>)>,
) -> Result<()> {
    let mut manual = std::collections::HashSet::new();
    let mut documents = std::collections::BTreeMap::new();
    for document in catalogue.drain(..) {
        let ghsa = document["ghsa_id"].as_str().ok_or(Refusal::Invalid)?.to_owned();
        documents.insert(ghsa, document);
    }
    for (data, osv_id) in overrides {
        let document: Value = serde_json::from_str(&data)?;
        let ghsa = document["ghsa_id"].as_str().ok_or(Refusal::Invalid)?.to_owned();
        if osv_id.is_none() { manual.insert(ghsa.clone()); }
        documents.insert(ghsa, document);
    }
    let mut remove = Vec::new();
    for (id, record) in records.iter() {
        if manual.contains(record.global_document()?["ghsa_id"].as_str().unwrap()) { remove.push(id.clone()); }
    }
    for id in remove { records.remove(&id); }
    *catalogue = documents.into_values().collect();
    Ok(())
}

pub(crate) fn matches(
    snapshots: &[Snapshot],
    advisories: &[Value],
    osv: &std::collections::BTreeMap<String, crate::backend::vulnerabilities::Advisory>,
) -> Result<Vec<(Package, Value, Value)>> {
    use crate::backend::vulnerabilities;
    // Catalogue metadata has one winner per GHSA. Matching retains every indexed OSV
    // record's affected packages and withdrawal state, including older alias records.
    let display: std::collections::HashMap<&str, &Value> = advisories.iter().filter_map(|document| document["ghsa_id"].as_str().map(|id| (id, document))).collect();
    let mut sources = Vec::new();
    let mut represented = std::collections::HashSet::new();
    for record in osv.values() {
        let mut document = record.global_document()?;
        let ghsa = document["ghsa_id"].as_str().unwrap().to_owned();
        represented.insert(ghsa.clone());
        if let Some(display) = display.get(ghsa.as_str()) {
            for (key, value) in display.as_object().ok_or(Refusal::Invalid)? {
                if key != "vulnerabilities" && key != "withdrawn_at" { document[key] = value.clone(); }
            }
        }
        sources.push((document, Some(record)));
    }
    for document in advisories {
        if !represented.contains(document["ghsa_id"].as_str().unwrap_or("")) { sources.push((document.clone(), None)); }
    }
    let normalized = |ecosystem: &str, name: &str| {
        vulnerabilities::name(vulnerabilities::ecosystem(ecosystem).unwrap_or(""), name)
    };
    // Advisory entries by ecosystem and normalized package name, so each dependency is compared
    // only with the entries that name it.
    let mut candidates: std::collections::HashMap<(String, String), Vec<_>> =
        std::collections::HashMap::new();
    for (advisory, osv) in &sources {
        let osv = *osv;
        if !advisory["withdrawn_at"].is_null()
            || (advisory["severity"] == "unknown" && osv.is_none())
        {
            continue;
        }
        for vulnerability in advisory["vulnerabilities"].as_array().into_iter().flatten() {
            let ecosystem = vulnerability["package"]["ecosystem"]
                .as_str()
                .unwrap_or_default();
            let name = vulnerability["package"]["name"]
                .as_str()
                .unwrap_or_default();
            candidates
                .entry((ecosystem.to_owned(), normalized(ecosystem, name)))
                .or_default()
                .push((advisory, osv, vulnerability));
        }
    }
    let mut matches: std::collections::BTreeMap<_, (Package, Value, Value)> = std::collections::BTreeMap::new();
    for mut package in dependency_graph::packages(snapshots) {
        package.ecosystem = ecosystem(&package.ecosystem).into();
        let key = (
            package.ecosystem.clone(),
            normalized(&package.ecosystem, &package.name),
        );
        for (advisory, osv, vulnerability) in candidates.get(&key).into_iter().flatten() {
            let range = vulnerability["vulnerable_version_range"]
                .as_str()
                .unwrap_or("");
            let matched = if let Some(osv) = osv {
                let locked = crate::backend::repositories::dependency_graph::Package {
                    manifest: package.manifest.clone(),
                    ecosystem: package.ecosystem.clone(),
                    name: package.name.clone(),
                    version: package.version.clone(),
                    package_url: package.package_url.clone().unwrap_or_default(),
                    scope: package.scope.clone(),
                };
                osv.matches(&locked)
                    // The advisory matched; this entry must match too, or another entry's
                    // range and fixed version would be reported.
                    && vulnerabilities::in_range(
                        if vulnerability["osv_range_type"] == "SEMVER" { "SEMVER" } else { vulnerabilities::ecosystem(&package.ecosystem).unwrap_or("") },
                        &package.version,
                        range,
                    )
            } else {
                affected(&package.version, range)
            };
            if !matched {
                continue;
            }
            let key = (
                advisory["ghsa_id"].as_str().unwrap_or("").to_owned(),
                package.manifest.clone(),
                package.ecosystem.clone(),
                package.name.clone(),
            );
            matches.entry(key).and_modify(|matched| {
                // Exact OSV versions can overlap intervals with a known fix. Prefer that
                // matching interval regardless of catalogue entry order, without borrowing
                // a fix from another installed version of the same dependency.
                if matched.0.version == package.version
                    && matched.2["first_patched_version"].as_str().is_none_or(str::is_empty)
                    && vulnerability["first_patched_version"].as_str().is_some_and(|version| !version.is_empty())
                {
                    matched.2 = (*vulnerability).clone();
                }
            }).or_insert_with(|| {
                (
                    package.clone(),
                    (*advisory).clone(),
                    (*vulnerability).clone(),
                )
            });
        }
    }
    Ok(matches.into_values().collect())
}

native_operation!(crate::backend::db::postgres::dependabot_alerts::global,crate::backend::db::sqlite::dependabot_alerts::global,
    fn global(pool:&DbPool)->Result<Vec<Value>>);
native_operation!(crate::backend::db::postgres::dependabot_alerts::without_osv,crate::backend::db::sqlite::dependabot_alerts::without_osv,
    fn without_osv(pool:&DbPool)->Result<Vec<Value>>);
native_operation!(crate::backend::db::postgres::dependabot_alerts::global_one,crate::backend::db::sqlite::dependabot_alerts::global_one,
    fn global_one(pool:&DbPool, ghsa_id:&str)->Result<Option<Value>>);
native_operation!(crate::backend::db::postgres::dependabot_alerts::import,crate::backend::db::sqlite::dependabot_alerts::import,
    fn import(pool:&DbPool,actor:i64,value:&Value)->Result<Value>);
native_operation!(crate::backend::db::postgres::dependabot_alerts::alerts,crate::backend::db::sqlite::dependabot_alerts::alerts,
    fn alerts(pool:&DbPool,actor:i64,repo:i64)->Result<Vec<Alert>>);
native_operation!(crate::backend::db::postgres::dependabot_alerts::update,crate::backend::db::sqlite::dependabot_alerts::update,
    fn update(pool:&DbPool,actor:i64,repo:i64,number:i64,input:&Update)->Result<Alert>);
native_operation!(crate::backend::db::postgres::dependabot_alerts::organization_repositories,crate::backend::db::sqlite::dependabot_alerts::organization_repositories,
    fn organization_repositories(pool:&DbPool,actor:i64,org:i64)->Result<Vec<i64>>);

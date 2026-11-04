use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_model::advisories::Advisory;
use codeyard_model::advisories::Data;
use codeyard_model::advisories::Refusal;
use codeyard_model::advisory_documents::import_document;
use codeyard_model::dependency_graph::Package;
use codeyard_model::dependency_graph::Snapshot;
use serde_json::json;
use serde_json::Value;
pub(crate) fn manifest_digest(manifest: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(manifest.as_bytes()))
}

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

pub fn published(row: Advisory) -> Result<Value> {
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
    let keys: std::collections::BTreeSet<(&str, String)> =
        codeyard_model::dependency_graph::packages(snapshots)
            .iter()
            .filter_map(|package| {
                let ecosystem =
                    codeyard_model::vulnerabilities::ecosystem(ecosystem(&package.ecosystem))?;
                Some((
                    ecosystem,
                    codeyard_model::vulnerabilities::name(ecosystem, &package.name),
                ))
            })
            .collect();
    json!(keys).to_string()
}

pub fn ecosystem(purl: &str) -> &str {
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

pub(crate) fn osv_records(
    rows: Vec<String>,
) -> Result<(
    std::collections::BTreeMap<String, codeyard_model::vulnerabilities::Advisory>,
    Vec<String>,
)> {
    let mut records = std::collections::BTreeMap::new();
    let mut aliases = std::collections::BTreeSet::new();
    for data in rows {
        let record: codeyard_model::vulnerabilities::Advisory = serde_json::from_str(&data)?;
        aliases.insert(
            record.global_document()?["ghsa_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
        records.insert(record.id.clone(), record);
    }
    Ok((records, aliases.into_iter().collect()))
}

pub(crate) fn catalogue_for_records(
    catalogue: &mut Vec<Value>,
    records: &mut std::collections::BTreeMap<String, codeyard_model::vulnerabilities::Advisory>,
    overrides: Vec<(String, Option<String>)>,
) -> Result<()> {
    let mut manual = std::collections::HashSet::new();
    let mut documents = std::collections::BTreeMap::new();
    for document in catalogue.drain(..) {
        let ghsa = document["ghsa_id"]
            .as_str()
            .ok_or(Refusal::Invalid)?
            .to_owned();
        documents.insert(ghsa, document);
    }
    for (data, osv_id) in overrides {
        let document: Value = serde_json::from_str(&data)?;
        let ghsa = document["ghsa_id"]
            .as_str()
            .ok_or(Refusal::Invalid)?
            .to_owned();
        if osv_id.is_none() {
            manual.insert(ghsa.clone());
        }
        documents.insert(ghsa, document);
    }
    let mut remove = Vec::new();
    for (id, record) in records.iter() {
        if manual.contains(record.global_document()?["ghsa_id"].as_str().unwrap()) {
            remove.push(id.clone());
        }
    }
    for id in remove {
        records.remove(&id);
    }
    *catalogue = documents.into_values().collect();
    Ok(())
}

pub fn matches(
    snapshots: &[Snapshot],
    advisories: &[Value],
    osv: &std::collections::BTreeMap<String, codeyard_model::vulnerabilities::Advisory>,
) -> Result<Vec<(Package, Value, Value)>> {
    // Catalogue metadata has one winner per GHSA. Matching retains every indexed OSV
    // record's affected packages and withdrawal state, including older alias records.
    let display: std::collections::HashMap<&str, &Value> = advisories
        .iter()
        .filter_map(|document| document["ghsa_id"].as_str().map(|id| (id, document)))
        .collect();
    let mut sources = Vec::new();
    let mut represented = std::collections::HashSet::new();
    for record in osv.values() {
        let mut document = record.global_document()?;
        let ghsa = document["ghsa_id"].as_str().unwrap().to_owned();
        represented.insert(ghsa.clone());
        if let Some(display) = display.get(ghsa.as_str()) {
            for (key, value) in display.as_object().ok_or(Refusal::Invalid)? {
                if key != "vulnerabilities" && key != "withdrawn_at" {
                    document[key] = value.clone();
                }
            }
        }
        sources.push((document, Some(record)));
    }
    for document in advisories {
        if !represented.contains(document["ghsa_id"].as_str().unwrap_or("")) {
            sources.push((document.clone(), None));
        }
    }
    let normalized = |ecosystem: &str, name: &str| {
        codeyard_model::vulnerabilities::name(
            codeyard_model::vulnerabilities::ecosystem(ecosystem).unwrap_or(""),
            name,
        )
    };
    // Advisory entries by ecosystem and normalized package name, so each dependency is compared
    // only with the entries that name it.
    let mut candidates: std::collections::HashMap<(String, String), Vec<_>> =
        std::collections::HashMap::new();
    for (advisory, osv) in &sources {
        let osv = *osv;
        if !advisory["withdrawn_at"].is_null() {
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
    let mut matches: std::collections::BTreeMap<_, (Package, Value, Value)> =
        std::collections::BTreeMap::new();
    for mut package in codeyard_model::dependency_graph::packages(snapshots) {
        package.ecosystem = ecosystem(&package.ecosystem).into();
        let key = (
            package.ecosystem.clone(),
            normalized(&package.ecosystem, &package.name),
        );
        for (advisory, osv, vulnerability) in candidates.get(&key).into_iter().flatten() {
            let range = vulnerability["vulnerable_version_range"]
                .as_str()
                .unwrap_or("");
            // Every entry uses the ecosystem's version ordering, with or without OSV backing,
            // except OSV SEMVER ranges. The entry must match too, or another entry's range and
            // fixed version are reported.
            let ordering = if osv.is_some() && vulnerability["osv_range_type"] == "SEMVER" {
                "SEMVER"
            } else {
                codeyard_model::vulnerabilities::ecosystem(&package.ecosystem).unwrap_or("")
            };
            if !codeyard_model::vulnerabilities::in_range(ordering, &package.version, range) {
                continue;
            }
            if let Some(osv) = osv {
                let locked = codeyard_model::repositories::Package {
                    manifest: package.manifest.clone(),
                    ecosystem: package.ecosystem.clone(),
                    name: package.name.clone(),
                    version: package.version.clone(),
                    package_url: package.package_url.clone().unwrap_or_default(),
                    scope: package.scope.clone(),
                };
                if !osv.matches(&locked) {
                    continue;
                }
            }
            let key = (
                advisory["ghsa_id"].as_str().unwrap_or("").to_owned(),
                package.manifest.clone(),
                package.ecosystem.clone(),
                package.name.clone(),
            );
            matches
                .entry(key)
                .and_modify(|matched| {
                    // Exact OSV versions can overlap intervals with a known fix. Prefer that
                    // matching interval regardless of catalogue entry order, without borrowing
                    // a fix from another installed version of the same dependency.
                    if matched.0.version == package.version
                        && matched.2["first_patched_version"]
                            .as_str()
                            .is_none_or(str::is_empty)
                        && vulnerability["first_patched_version"]
                            .as_str()
                            .is_some_and(|version| !version.is_empty())
                    {
                        matched.2 = (*vulnerability).clone();
                    }
                })
                .or_insert_with(|| {
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

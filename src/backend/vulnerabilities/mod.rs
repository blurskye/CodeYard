//! OSV mirrors and repository alerts share the pinned dependency inventory.
pub(crate) mod versions;
use crate::backend::{
    app::AppState,
    db,
    repositories::{
        dependency_graph::{self, Package},
        store::ErrorKind,
    },
};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{cmp::Ordering, io::Read, sync::Arc, time::Duration};

pub const ECOSYSTEMS: &[&str] = &[
    "npm",
    "PyPI",
    "crates.io",
    "Go",
    "Maven",
    "RubyGems",
    "Packagist",
    "NuGet",
    "Pub",
    "Hex",
];
pub fn ecosystem(name: &str) -> Option<&'static str> {
    Some(match name {
        "npm" => "npm",
        "cargo" | "rust" => "crates.io",
        "golang" | "go" => "Go",
        "pypi" | "pip" => "PyPI",
        "gem" | "rubygems" => "RubyGems",
        "composer" => "Packagist",
        "maven" => "Maven",
        "nuget" => "NuGet",
        "pub" => "Pub",
        "hex" => "Hex",
        _ => return None,
    })
}
/// Python package names compare case-insensitively with each run of `-`, `_` and `.`
/// read as one hyphen.
pub(crate) fn python_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}
pub(crate) fn name(ecosystem: &str, name: &str) -> String {
    match ecosystem {
        "PyPI" => python_name(name),
        "NuGet" => name.to_ascii_lowercase(),
        "Maven" => name.replace('/', ":"),
        _ => name.into(),
    }
}
/// Whether `version` lies in a range as advisory documents write it, such as
/// `>= 1.0.0, < 1.2.0 || = 2.0.1`, under the ecosystem's version ordering.
pub(crate) fn in_range(ecosystem: &str, version: &str, range: &str) -> bool {
    range.split("||").any(|part| {
        let bounds: Vec<&str> = part.split(',').map(str::trim).collect();
        !bounds.iter().any(|bound| bound.is_empty())
            && bounds.iter().all(|bound| {
                let (operator, value) = [">=", "<=", ">", "<", "="]
                    .into_iter()
                    .find_map(|operator| {
                        bound
                            .strip_prefix(operator)
                            .map(|value| (operator, value.trim()))
                    })
                    .unwrap_or(("=", bound));
                if operator == ">=" && value == "0" {
                    return true;
                }
                let Some(order) = versions::compare(ecosystem, version, value) else {
                    return false;
                };
                match operator {
                    ">=" => order != Ordering::Less,
                    "<=" => order != Ordering::Greater,
                    ">" => order == Ordering::Greater,
                    "<" => order == Ordering::Less,
                    _ => order == Ordering::Equal,
                }
            })
    })
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Advisory {
    pub id: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub details: String,
    #[serde(default)]
    pub severity: Vec<Value>,
    #[serde(default)]
    pub affected: Vec<Affected>,
    #[serde(default)]
    pub references: Vec<Value>,
    pub published: Option<chrono::DateTime<chrono::Utc>>,
    pub modified: chrono::DateTime<chrono::Utc>,
    pub withdrawn: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub database_specific: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Affected {
    pub package: OsvPackage,
    #[serde(default)]
    pub ranges: Vec<Range>,
    #[serde(default)]
    pub versions: Vec<String>,
    #[serde(default)]
    pub ecosystem_specific: Value,
    #[serde(default)]
    pub database_specific: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OsvPackage {
    pub ecosystem: String,
    pub name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Range {
    pub r#type: String,
    #[serde(default)]
    pub events: Vec<Value>,
}
impl Advisory {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "OSV record exceeds its limit"
        );
        let record: Self = serde_json::from_slice(bytes)?;
        ensure!(
            !record.id.is_empty()
                && record.id.len() <= 256
                && !record.id.chars().any(char::is_control)
                && record.affected.len() <= 2000,
            "Invalid OSV record"
        );
        Ok(record)
    }
    pub fn matches(&self, package: &Package) -> bool {
        if self.withdrawn.is_some() {
            return false;
        }
        let Some(ecosystem) = ecosystem(&package.ecosystem) else {
            return false;
        };
        self.affected.iter().any(|affected| {
            if affected.package.ecosystem != ecosystem
                || name(ecosystem, &affected.package.name) != name(ecosystem, &package.name)
            {
                return false;
            }
            if affected
                .versions
                .iter()
                .any(|v| versions::compare(ecosystem, &package.version, v) == Some(Ordering::Equal))
            {
                return true;
            }
            affected
                .ranges
                .iter()
                .filter(|r| matches!(r.r#type.as_str(), "SEMVER" | "ECOSYSTEM"))
                .any(|range| {
                    let mut introduced = None;
                    for event in &range.events {
                        if let Some(start) = event["introduced"].as_str() {
                            introduced = Some(start);
                            continue;
                        }
                        if let Some(start) = introduced {
                            let lower = start == "0"
                                || versions::compare(ecosystem, &package.version, start)
                                    .is_some_and(|o| o != Ordering::Less);
                            let upper = if let Some(end) =
                                event["fixed"].as_str().or_else(|| event["limit"].as_str())
                            {
                                versions::compare(ecosystem, &package.version, end)
                                    == Some(Ordering::Less)
                            } else if let Some(end) = event["last_affected"].as_str() {
                                versions::compare(ecosystem, &package.version, end)
                                    .is_some_and(|o| o != Ordering::Greater)
                            } else {
                                false
                            };
                            if lower && upper {
                                return true;
                            }
                            introduced = None;
                        }
                    }
                    introduced.is_some_and(|start| {
                        start == "0"
                            || versions::compare(ecosystem, &package.version, start)
                                .is_some_and(|o| o != Ordering::Less)
                    })
                })
        })
    }
    pub fn severity(&self) -> String {
        // Labels other than these fall through to the scores below.
        match self.database_specific["severity"]
            .as_str()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("moderate" | "medium") => return "medium".into(),
            Some(label @ ("low" | "high" | "critical")) => return label.into(),
            _ => {}
        }
        for severity in &self.severity {
            if let Some(score) = severity["score"]
                .as_str()
                .and_then(|v| v.parse::<f64>().ok())
            {
                return if score >= 9.0 {
                    "critical"
                } else if score >= 7.0 {
                    "high"
                } else if score >= 4.0 {
                    "medium"
                } else {
                    "low"
                }
                .into();
            }
            if let Some(vector) = severity["score"].as_str() {
                if let Ok(score) = db::advisories::cvss_score(vector) {
                    return if score >= 9.0 {
                        "critical"
                    } else if score >= 7.0 {
                        "high"
                    } else if score >= 4.0 {
                        "medium"
                    } else {
                        "low"
                    }
                    .into();
                }
            }
        }
        "unknown".into()
    }
    pub fn global_document(&self) -> Result<Value> {
        use sha2::{Digest, Sha256};
        let mut value = self.document();
        if value["ghsa_id"].is_null() {
            // OSV records without a GHSA alias need a stable local catalogue identity.
            let hash = Sha256::digest(self.id.as_bytes());
            let alphabet = b"23456789cfghjmpqrvwx";
            let id: String = hash[..12]
                .iter()
                .map(|b| alphabet[*b as usize % alphabet.len()] as char)
                .collect();
            value["ghsa_id"] = json!(format!("GHSA-{}-{}-{}", &id[..4], &id[4..8], &id[8..]));
        }
        if self.summary.trim().is_empty() {
            value["summary"] = json!(self.id);
        }
        value["published_at"] = json!(self.published.unwrap_or(self.modified));
        value["references"] = json!(self
            .references
            .iter()
            .filter_map(|r| r["url"].as_str())
            .collect::<Vec<_>>());
        if value["cvss"]["vector_string"].is_null() {
            value["cvss"] = Value::Null;
        }
        value["vulnerabilities"] = json!(self.affected.iter().map(|affected| {
            let mut ranges: Vec<String> = affected.versions.iter().map(|v| format!("= {v}")).collect();
            for range in &affected.ranges {
                if !matches!(range.r#type.as_str(), "SEMVER" | "ECOSYSTEM") {
                    continue;
                }
                let mut lower = None;
                for event in &range.events {
                    if let Some(start) = event["introduced"].as_str() {
                        lower = Some(format!(">= {start}"));
                    } else if let Some(start) = lower.take() {
                        if let Some(end) = event["fixed"].as_str().or_else(|| event["limit"].as_str()) {
                            ranges.push(format!("{start}, < {end}"));
                        } else if let Some(end) = event["last_affected"].as_str() {
                            ranges.push(format!("{start}, <= {end}"));
                        }
                    }
                }
                if let Some(start) = lower { ranges.push(start); }
            }
            let ecosystem = match affected.package.ecosystem.as_str() {
                "crates.io" => "rust", "PyPI" => "pip", "Go" => "go",
                "RubyGems" => "rubygems", "Packagist" => "composer", "Maven" => "maven",
                "NuGet" => "nuget", "Pub" => "pub", "Hex" => "hex", other => other,
            };
            // A GIT range's fix is a commit ID, not a package version.
            let patched = affected
                .ranges
                .iter()
                .filter(|r| matches!(r.r#type.as_str(), "SEMVER" | "ECOSYSTEM"))
                .flat_map(|r| &r.events)
                .find_map(|e| e["fixed"].as_str());
            json!({"package":{"ecosystem":ecosystem,"name":affected.package.name},"vulnerable_version_range":if ranges.is_empty() {"unknown".into()} else {ranges.join(" || ")},"first_patched_version":patched,"vulnerable_functions":[]})
        }).collect::<Vec<_>>());
        db::dependabot_alerts::import_document(&value)
    }
    pub fn document(&self) -> Value {
        let ghsa = self
            .aliases
            .iter()
            .chain(std::iter::once(&self.id))
            .find(|s| s.starts_with("GHSA-"));
        let cve = self
            .aliases
            .iter()
            .chain(std::iter::once(&self.id))
            .find(|s| s.starts_with("CVE-"));
        let vector = self
            .severity
            .iter()
            .find_map(|s| s["score"].as_str().filter(|s| s.starts_with("CVSS:3.")));
        let score = vector.and_then(|s| db::advisories::cvss_score(s).ok());
        json!({"ghsa_id":ghsa,"cve_id":cve,"summary":self.summary,"description":self.details,"severity":self.severity(),"identifiers":self.aliases.iter().chain(std::iter::once(&self.id)).filter(|id|id.starts_with("CVE-")||id.starts_with("GHSA-")).map(|id|json!({"type":if id.starts_with("CVE-"){"CVE"}else{"GHSA"},"value":id})).collect::<Vec<_>>(),"references":self.references.iter().filter_map(|r|r["url"].as_str().map(|url|json!({"url":url}))).collect::<Vec<_>>(),"published_at":self.published,"updated_at":self.modified,"withdrawn_at":self.withdrawn,"vulnerabilities":[],"cvss":{"score":score,"vector_string":vector},"cwes":[]})
    }
}

pub async fn ingest(pool: &db::DbPool, bytes: &[u8]) -> Result<bool> {
    let changed = db::vulnerabilities::upsert(pool, &Advisory::parse(bytes)?).await?;
    if changed {
        db::vulnerabilities::rescan_all(pool).await?;
    }
    Ok(changed)
}
pub async fn findings(pool: &db::DbPool, package: &Package) -> Result<Vec<Advisory>> {
    let Some(ecosystem) = ecosystem(&package.ecosystem) else {
        return Ok(Vec::new());
    };
    ensure!(
        versions::compare(ecosystem, &package.version, &package.version).is_some(),
        "Unsupported locked package version"
    );
    let rows =
        db::vulnerabilities::advisories(pool, ecosystem, &name(ecosystem, &package.name)).await?;
    rows.into_iter()
        .map(|row| serde_json::from_str::<Advisory>(&row).map_err(Into::into))
        .filter_map(|r| match r {
            Ok(a) if a.matches(package) => Some(Ok(a)),
            Ok(_) => None,
            Err(e) => Some(Err(e)),
        })
        .collect()
}
pub async fn scan(state: &AppState, repo: &db::models::Repository, generation: i64) -> Result<()> {
    let reader = db::dependency_graph::Reader::Scan(generation);
    let reference = format!("refs/heads/{}", repo.default_branch);
    let (sha, packages) = match dependency_graph::native_inventory(state, &reader, repo, &reference).await {
        Ok(result) => result,
        Err(error) if matches!(error.kind, ErrorKind::NotFound | ErrorKind::MissingPath) => {
            return Ok(())
        }
        Err(error) => return Err(error.into()),
    };
    let snapshot = dependency_graph::snapshot(sha, reference, packages);
    db::vulnerabilities::save_snapshot(&state.pool, repo.id, generation, &snapshot).await
}
async fn sync(state: &AppState) -> Result<()> {
    let settings = db::vulnerabilities::settings(&state.pool).await?;
    synchronize(&state.pool, &settings).await
}
/// Downloads each enabled ecosystem's `all.zip` from the OSV mirror and stores its records.
pub async fn synchronize(
    pool: &db::DbPool,
    settings: &db::vulnerabilities::Settings,
) -> Result<()> {
    let enabled: Vec<String> = serde_json::from_str(&settings.ecosystems)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    for ecosystem in enabled {
        ensure!(
            ECOSYSTEMS.contains(&ecosystem.as_str()),
            "Unknown OSV ecosystem"
        );
        let url = format!(
            "{}/{}/all.zip",
            settings.base_url.trim_end_matches('/'),
            ecosystem
        );
        let mut response = client.get(url).send().await?.error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= 256 * 1024 * 1024,
                "OSV archive exceeds its limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        let records = tokio::task::spawn_blocking(move || -> Result<Vec<Advisory>> {
            let mut archive = crate::backend::zip_archives::open(bytes, 1_000_000)?;
            let mut records = Vec::new();
            let mut expanded = 0u64;
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i)?;
                if entry.is_dir() || !entry.name().ends_with(".json") {
                    continue;
                }
                ensure!(
                    entry.size() <= 4 * 1024 * 1024,
                    "OSV record exceeds its limit"
                );
                expanded += entry.size();
                ensure!(
                    expanded <= 2 * 1024 * 1024 * 1024,
                    "Expanded OSV archive exceeds its limit"
                );
                let mut bytes = Vec::new();
                entry
                    .by_ref()
                    .take(4 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)?;
                // One record the catalogue cannot hold must not stop the rest of the archive.
                match Advisory::parse(&bytes) {
                    Ok(record) => records.push(record),
                    Err(error) => {
                        tracing::warn!(entry = entry.name(), %error, "OSV record was skipped")
                    }
                }
            }
            Ok(records)
        })
        .await??;
        // Repositories are rescanned once per archive, not once per changed record.
        let mut changed = false;
        for record in records {
            if let Err(error) = record.global_document() {
                tracing::warn!(advisory = %record.id, %error, "OSV record was skipped");
                continue;
            }
            changed |= db::vulnerabilities::upsert(pool, &record).await?;
        }
        if changed {
            db::vulnerabilities::rescan_all(pool).await?;
        }
    }
    Ok(())
}
pub async fn run(state: Arc<AppState>, mut stop: tokio::sync::watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {_ = stop.changed()=>break,_=interval.tick()=>{}}
        let work = async {
            if db::vulnerabilities::claim_sync(&state.pool).await? {
                if let Err(error) = sync(&state).await {
                    tracing::warn!(%error,"OSV synchronization failed");
                }
            }
            db::vulnerabilities::queue_changed(&state.pool).await?;
            for _ in 0..20 {
                let Some((repo, generation)) = db::vulnerabilities::claim(&state.pool).await?
                else {
                    break;
                };
                if let Some(repo) =
                    db::repositories::records::get_repository_by_id(&state.pool, repo).await?
                {
                    if let Err(error) = scan(&state, &repo, generation).await {
                        tracing::warn!(repo_id=repo.id,%error,"Dependency vulnerability scan failed");
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::select! {_ = stop.changed()=>break,result=work=>if let Err(error)=result{tracing::warn!(%error,"Vulnerability scanning job failed");}}
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranges_are_checked_one_entry_at_a_time() {
        assert!(in_range("crates.io", "1.1.0", ">= 1.0.0, < 1.2.0"));
        assert!(!in_range("crates.io", "1.1.0", ">= 2.0.0, < 3.0.0"));
        assert!(in_range("crates.io", "2.0.1", ">= 2.1.0 || = 2.0.1"));
        assert!(in_range("crates.io", "0.1.0", ">= 0, < 1.0.0"));
        assert!(!in_range("crates.io", "1.0.0", "unknown"));
    }
    #[test]
    fn osv_catalogue_and_snapshot_matching() {
        let mut advisory = Advisory::parse(include_bytes!("fixtures/OSV-2026-1.json")).unwrap();
        advisory.aliases.clear();
        advisory.affected[0].package = OsvPackage {
            ecosystem: "PyPI".into(),
            name: "Example_Package".into(),
        };
        advisory.affected[0].ranges[0].r#type = "ECOSYSTEM".into();
        advisory.affected[0].ranges[0].events =
            vec![json!({"introduced":"0"}), json!({"fixed":"1.0"})];
        let document = advisory.global_document().unwrap();
        assert_eq!(
            document["ghsa_id"],
            advisory.global_document().unwrap()["ghsa_id"]
        );
        assert_eq!(
            document["references"],
            json!(["https://example.test/advisory"])
        );
        let package = Package {
            manifest: "requirements.txt".into(),
            ecosystem: "pypi".into(),
            name: "example-package".into(),
            version: "1.0rc1".into(),
            package_url: "pkg:pypi/example-package@1.0rc1".into(),
            scope: "runtime".into(),
        };
        let snapshot =
            dependency_graph::snapshot("a".repeat(40), "refs/heads/main".into(), vec![package]);
        snapshot.validate().unwrap();
        let mut osv = std::collections::BTreeMap::from([(
            document["ghsa_id"].as_str().unwrap().to_owned(),
            advisory,
        )]);
        let matches = db::dependabot_alerts::matches(
            std::slice::from_ref(&snapshot),
            std::slice::from_ref(&document),
            &osv,
        );
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0.version, "1.0rc1");
        osv.values_mut().next().unwrap().withdrawn = Some(chrono::Utc::now());
        assert!(db::dependabot_alerts::matches(&[snapshot], &[document], &osv).is_empty());
    }
    #[test]
    fn local_maven_advisories_match_namespaced_package_urls() {
        let document = json!({"ghsa_id":"GHSA-aaaa-bbbb-cccc","severity":"high","withdrawn_at":null,
            "vulnerabilities":[{"package":{"ecosystem":"maven","name":"org.example:parser"},
                "vulnerable_version_range":"< 2.0.0"}]});
        let package = Package {
            manifest: "pom.xml".into(),
            ecosystem: "maven".into(),
            name: "org.example/parser".into(),
            version: "1.0.0".into(),
            package_url: "pkg:maven/org.example/parser@1.0.0".into(),
            scope: "runtime".into(),
        };
        let snapshot =
            dependency_graph::snapshot("a".repeat(40), "refs/heads/main".into(), vec![package]);
        let matches = db::dependabot_alerts::matches(
            &[snapshot],
            &[document],
            &std::collections::BTreeMap::new(),
        );
        assert_eq!(matches.len(), 1);
    }
    #[test]
    fn osv_ranges_and_withdrawal() {
        let mut a = Advisory::parse(include_bytes!("fixtures/OSV-2026-1.json")).unwrap();
        let mut p = Package {
            manifest: "Cargo.lock".into(),
            ecosystem: "cargo".into(),
            name: "example".into(),
            version: "1.0.0".into(),
            package_url: "pkg:cargo/example@1.0.0".into(),
            scope: "runtime".into(),
        };
        assert!(a.matches(&p));
        p.version = "1.2.0".into();
        assert!(!a.matches(&p));
        p.version = "1.2.0-rc.1".into();
        assert!(a.matches(&p));
        a.withdrawn = Some(chrono::Utc::now());
        assert!(!a.matches(&p));
    }
}

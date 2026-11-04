use crate::repositories::Package;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cmp::Ordering;
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
pub const MAX_AFFECTED_PACKAGES: usize = 2000;
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
pub fn python_name(name: &str) -> String {
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

pub fn name(ecosystem: &str, name: &str) -> String {
    match ecosystem {
        "PyPI" => python_name(name),
        "NuGet" => name.to_ascii_lowercase(),
        "Maven" => name.replace('/', ":"),
        _ => name.into(),
    }
}

/// Whether `version` lies in a range as advisory documents write it, such as
/// `>= 1.0.0, < 1.2.0 || = 2.0.1`, under the ecosystem's version ordering.
pub fn in_range(ecosystem: &str, version: &str, range: &str) -> bool {
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
                let Some(order) = crate::versions::compare(ecosystem, version, value) else {
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

/// The validated catalogue projection paired with its original OSV record.
pub struct PreparedAdvisory {
    pub advisory: Advisory,
    pub document: Value,
}

impl Advisory {
    pub fn prepare(self) -> Result<PreparedAdvisory> {
        let document = self.global_document()?;
        Ok(PreparedAdvisory {
            advisory: self,
            document,
        })
    }

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
                && record.affected.len() <= MAX_AFFECTED_PACKAGES
                && record
                    .affected
                    .iter()
                    .all(|affected| !affected.package.name.contains('\0')
                        && !affected.package.ecosystem.contains('\0')),
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
            if affected.versions.iter().any(|v| {
                crate::versions::compare(ecosystem, &package.version, v) == Some(Ordering::Equal)
            }) {
                return true;
            }
            affected
                .ranges
                .iter()
                .filter(|r| matches!(r.r#type.as_str(), "SEMVER" | "ECOSYSTEM"))
                .any(|range| {
                    let ecosystem = if range.r#type == "SEMVER" {
                        "SEMVER"
                    } else {
                        ecosystem
                    };
                    let mut introduced = None;
                    for event in &range.events {
                        if let Some(start) = event["introduced"].as_str() {
                            introduced = Some(start);
                            continue;
                        }
                        if let Some(start) = introduced {
                            let lower = start == "0"
                                || crate::versions::compare(ecosystem, &package.version, start)
                                    .is_some_and(|o| o != Ordering::Less);
                            let upper = if let Some(end) =
                                event["fixed"].as_str().or_else(|| event["limit"].as_str())
                            {
                                crate::versions::compare(ecosystem, &package.version, end)
                                    == Some(Ordering::Less)
                            } else if let Some(end) = event["last_affected"].as_str() {
                                crate::versions::compare(ecosystem, &package.version, end)
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
                            || crate::versions::compare(ecosystem, &package.version, start)
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
                if let Ok(score) = crate::advisories::cvss_score(vector) {
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
    pub fn catalogue_id(&self) -> String {
        use sha2::{Digest, Sha256};
        if let Some(id) = self
            .aliases
            .iter()
            .chain(std::iter::once(&self.id))
            .find(|id| id.starts_with("GHSA-"))
        {
            return id.clone();
        }
        let hash = Sha256::digest(self.id.as_bytes());
        let alphabet = b"23456789cfghjmpqrvwx";
        let id: String = hash[..12]
            .iter()
            .map(|b| alphabet[*b as usize % alphabet.len()] as char)
            .collect();
        format!("GHSA-{}-{}-{}", &id[..4], &id[4..8], &id[8..])
    }

    pub fn global_document(&self) -> Result<Value> {
        let mut value = self.document();
        value["ghsa_id"] = json!(self.catalogue_id());
        if self.summary.trim().is_empty() {
            value["summary"] = json!(self.id);
        }
        let bounded = |text: &str, limit: usize| {
            let mut end = text.len().min(limit);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text[..end].to_owned()
        };
        value["summary"] = json!(bounded(value["summary"].as_str().unwrap_or(&self.id), 1024));
        value["description"] = json!(bounded(&self.details, 65535));
        value["published_at"] = json!(self.published.unwrap_or(self.modified));
        value["references"] = json!(self
            .references
            .iter()
            .filter_map(|r| r["url"].as_str())
            .collect::<Vec<_>>());
        if value["cvss"]["vector_string"].is_null() {
            value["cvss"] = Value::Null;
        }
        let mut vulnerabilities = Vec::new();
        for affected in &self.affected {
            let ecosystem = match affected.package.ecosystem.as_str() {
                "crates.io" => "rust",
                "PyPI" => "pip",
                "Go" => "go",
                "RubyGems" => "rubygems",
                "Packagist" => "composer",
                "Maven" => "maven",
                "NuGet" => "nuget",
                "Pub" => "pub",
                "Hex" => "hex",
                other => other,
            };
            let mut entry = |range: String, patched: Option<&str>, kind: &str| {
                vulnerabilities.push(json!({"package":{"ecosystem":ecosystem,"name":affected.package.name},"vulnerable_version_range":range,"first_patched_version":patched,"vulnerable_functions":[],"osv_range_type":kind}));
            };
            for version in &affected.versions {
                entry(format!("= {version}"), None, "ECOSYSTEM");
            }
            for range in &affected.ranges {
                if !matches!(range.r#type.as_str(), "SEMVER" | "ECOSYSTEM") {
                    continue;
                }
                let mut lower = None;
                for event in &range.events {
                    if let Some(start) = event["introduced"].as_str() {
                        lower = Some(format!(">= {start}"));
                    } else if let Some(start) = lower.take() {
                        if let Some(end) =
                            event["fixed"].as_str().or_else(|| event["limit"].as_str())
                        {
                            entry(
                                format!("{start}, < {end}"),
                                event["fixed"].as_str(),
                                &range.r#type,
                            );
                        } else if let Some(end) = event["last_affected"].as_str() {
                            entry(format!("{start}, <= {end}"), None, &range.r#type);
                        }
                    }
                }
                if let Some(start) = lower {
                    entry(start, None, &range.r#type);
                }
            }
        }
        value["vulnerabilities"] = json!(vulnerabilities);
        crate::advisory_documents::osv_document(&value)
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
        let vector = vector.filter(|s| crate::advisories::cvss_score(s).is_ok());
        let score = vector.and_then(|s| crate::advisories::cvss_score(s).ok());
        json!({"ghsa_id":ghsa,"cve_id":cve,"summary":self.summary,"description":self.details,"severity":self.severity(),"identifiers":self.aliases.iter().chain(std::iter::once(&self.id)).filter(|id|id.starts_with("CVE-")||id.starts_with("GHSA-")).map(|id|json!({"type":if id.starts_with("CVE-"){"CVE"}else{"GHSA"},"value":id})).collect::<Vec<_>>(),"references":self.references.iter().filter_map(|r|r["url"].as_str().map(|url|json!({"url":url}))).collect::<Vec<_>>(),"published_at":self.published,"updated_at":self.modified,"withdrawn_at":self.withdrawn,"vulnerabilities":[],"cvss":{"score":score,"vector_string":vector},"cwes":[]})
    }
}

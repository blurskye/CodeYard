//! Dependency submissions retain each detector's latest view without mixing commit histories.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub version: i64,
    pub job: Job,
    pub sha: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub detector: Detector,
    pub scanned: DateTime<Utc>,
    pub metadata: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    pub manifests: BTreeMap<String, Manifest>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub id: String,
    pub correlator: String,
    pub html_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Detector {
    pub name: String,
    pub version: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub file: Option<ManifestFile>,
    pub metadata: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    pub resolved: BTreeMap<String, Dependency>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    pub source_location: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub package_url: Option<String>,
    pub metadata: Option<BTreeMap<String, Value>>,
    pub relationship: Option<String>,
    pub scope: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

impl Snapshot {
    pub fn validate(&self) -> Result<(), crate::backend::db::dependency_graph::GraphError> {
        let valid_metadata = |metadata: &Option<BTreeMap<String, Value>>| {
            metadata.as_ref().is_none_or(|values| {
                values
                    .values()
                    .all(|v| v.is_string() || v.is_number() || v.is_boolean())
            })
        };
        let valid = self.version >= 0
            && (40..=64).contains(&self.sha.len())
            && self.sha.bytes().all(|c| c.is_ascii_hexdigit())
            && self.reference.starts_with("refs/")
            && git2::Reference::is_valid_name(&self.reference)
            // The reference, detector and correlator together form an index key, so they stay
            // within PostgreSQL's index entry size.
            && self.reference.len() <= 1024
            && !self.job.id.is_empty()
            && (1..=255).contains(&self.job.correlator.len())
            && (1..=255).contains(&self.detector.name.len())
            && valid_metadata(&self.metadata)
            && self.manifests.values().all(|manifest| {
                valid_metadata(&manifest.metadata)
                    && manifest.resolved.values().all(|dependency| {
                        dependency.package_url.as_deref().is_none_or(|p| {
                            crate::backend::db::dependency_inventory::package_parts(p).is_some()
                        }) && dependency
                            .relationship
                            .as_deref()
                            .is_none_or(|r| matches!(r, "direct" | "indirect"))
                            && dependency
                                .scope
                                .as_deref()
                                .is_none_or(|s| matches!(s, "runtime" | "development"))
                            && dependency
                                .dependencies
                                .iter()
                                .all(|id| manifest.resolved.contains_key(id))
                            && valid_metadata(&dependency.metadata)
                    })
            });
        if !valid || serde_json::to_vec(self).map_or(true, |bytes| bytes.len() > 1024 * 1024) {
            return Err(crate::backend::db::dependency_graph::GraphError::Invalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Package {
    pub manifest: String,
    pub ecosystem: String,
    pub name: String,
    pub version: String,
    pub package_url: Option<String>,
    pub scope: String,
}

fn decode(value: &str) -> Option<String> {
    // Form decoding treats '+' as a space, unlike a package URL.
    url::form_urlencoded::parse(format!("v={}", value.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
}

fn package(purl: &str) -> Option<(String, String, String)> {
    let path = purl.strip_prefix("pkg:")?.trim_start_matches('/');
    let path = path.split(['?', '#']).next()?;
    let (ecosystem, path) = path.split_once('/')?;
    if ecosystem.is_empty() || path.is_empty() {
        return None;
    }
    let (name, version) = path.rsplit_once('@').unwrap_or((path, ""));
    let ecosystem = match ecosystem {
        "cargo" => "rust",
        other => other,
    };
    Some((ecosystem.to_owned(), decode(name)?, decode(version)?))
}

pub fn packages(snapshots: &[Snapshot]) -> BTreeSet<Package> {
    let mut rows: BTreeMap<_, Package> = BTreeMap::new();
    for snapshot in snapshots {
        for manifest in snapshot.manifests.values() {
            let path = manifest
                .file
                .as_ref()
                .and_then(|f| f.source_location.as_ref())
                .unwrap_or(&manifest.name);
            for (key, dependency) in &manifest.resolved {
                let (ecosystem, name, version) = dependency
                    .package_url
                    .as_deref()
                    .and_then(package)
                    .unwrap_or_else(|| (String::new(), key.clone(), String::new()));
                let row = Package {
                    manifest: path.clone(),
                    ecosystem,
                    name,
                    version,
                    package_url: dependency.package_url.clone(),
                    scope: dependency.scope.clone().unwrap_or_else(|| "unknown".into()),
                };
                rows.entry((
                    row.manifest.clone(),
                    row.package_url.clone(),
                    row.name.clone(),
                    row.version.clone(),
                ))
                .and_modify(|known| {
                    if row.scope == "runtime" || known.scope == "unknown" {
                        known.scope = row.scope.clone();
                    }
                })
                .or_insert(row);
            }
        }
    }
    rows.into_values().collect()
}

pub fn diff(base: &[Snapshot], head: &[Snapshot], manifest: Option<&str>) -> Vec<Value> {
    let base = packages(base);
    let head = packages(head);
    let mut changes = Vec::new();
    for (kind, before, after) in [("removed", &base, &head), ("added", &head, &base)] {
        for package in before
            .difference(after)
            .filter(|p| manifest.is_none_or(|name| p.manifest == name))
        {
            let mut value = json!(package);
            value["change_type"] = json!(kind);
            value["license"] = Value::Null;
            value["source_repository_url"] = Value::Null;
            value["vulnerabilities"] = json!([]);
            changes.push(value);
        }
    }
    changes
}

pub fn sbom(name: &str, snapshots: &[Snapshot]) -> Value {
    let rows = packages(snapshots);
    let mut packages = Vec::new();
    let mut ids = BTreeMap::new();
    for row in &rows {
        let key = (
            row.package_url.clone(),
            row.name.clone(),
            row.version.clone(),
        );
        if ids.contains_key(&key) {
            continue;
        }
        let id = format!("SPDXRef-Package-{}", ids.len());
        ids.insert(key, id.clone());
        let mut value = json!({
            "SPDXID":id, "name":row.name, "versionInfo":row.version,
            "downloadLocation":"NOASSERTION", "filesAnalyzed":false,
            "licenseConcluded":"NOASSERTION", "licenseDeclared":"NOASSERTION"
        });
        if let Some(purl) = &row.package_url {
            value["externalRefs"] = json!([{"referenceCategory":"PACKAGE-MANAGER", "referenceType":"purl", "referenceLocator":purl}]);
        }
        packages.push(value);
    }
    let mut relationships = BTreeSet::new();
    // Child names are local to the manifest, not global package identifiers.
    for snapshot in snapshots {
        for manifest in snapshot.manifests.values() {
            let id_for = |key: &str, dependency: &Dependency| {
                let (_, name, version) = dependency
                    .package_url
                    .as_deref()
                    .and_then(package)
                    .unwrap_or_else(|| (String::new(), key.to_owned(), String::new()));
                ids.get(&(dependency.package_url.clone(), name, version))
                    .cloned()
            };
            for (key, dependency) in &manifest.resolved {
                let Some(id) = id_for(key, dependency) else {
                    continue;
                };
                relationships.insert(("SPDXRef-DOCUMENT".to_owned(), "DESCRIBES", id.clone()));
                for child in &dependency.dependencies {
                    if let Some((child_key, child_dependency)) =
                        manifest.resolved.get_key_value(child)
                    {
                        if let Some(child_id) = id_for(child_key, child_dependency) {
                            relationships.insert((id.clone(), "DEPENDS_ON", child_id));
                        }
                    }
                }
            }
        }
    }
    let relationships: Vec<_> = relationships
        .into_iter()
        .map(|(source, kind, target)| {
            json!({
                "spdxElementId":source, "relationshipType":kind, "relatedSpdxElement":target
            })
        })
        .collect();
    json!({"sbom": {
        "SPDXID":"SPDXRef-DOCUMENT", "spdxVersion":"SPDX-2.3", "dataLicense":"CC0-1.0",
        "name":name, "documentNamespace":format!("https://spdx.org/spdxdocs/{}", uuid::Uuid::new_v4()),
        "creationInfo":{"created":Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true), "creators":["Tool: Codeyard"]},
        "packages":packages, "relationships":relationships
    }})
}

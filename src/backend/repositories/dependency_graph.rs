//! Dependency inventories pin Git before reading either file and never execute package managers.
use super::{
    dependencies::npm::Lockfile,
    store::{repo_store, ErrorKind, RepoError},
};
use crate::backend::{
    app::AppState,
    db::{self, models::Repository},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Package {
    pub manifest: String,
    pub ecosystem: String,
    pub name: String,
    pub version: String,
    pub package_url: String,
    pub scope: String,
}

fn unavailable(_: anyhow::Error) -> RepoError {
    RepoError::new(
        ErrorKind::Unavailable,
        "Dependency graph storage unavailable",
    )
}

fn graph_error(error: db::dependency_graph::GraphError) -> RepoError {
    match error {
        error @ (db::dependency_graph::GraphError::Disabled | db::dependency_graph::GraphError::Limit) => invalid(error.to_string()),
        error => unavailable(error.into()),
    }
}

async fn file(
    store: &super::store::RepoStore,
    sha: &str,
    name: &str,
) -> Result<Option<Vec<u8>>, RepoError> {
    let limit = super::dependencies::npm::MAX_LOCK_BYTES as u64;
    match store
        .blob_range(
            sha,
            name,
            super::store::BlobRange::Window {
                offset: 0,
                length: limit,
            },
        )
        .await
    {
        // An oversized file is refused from its size, so the whole blob is never parsed.
        Ok(blob) if blob.size > limit => Err(invalid("Dependency file is too large")),
        Ok(blob) => Ok(Some(blob.bytes)),
        Err(error) if matches!(error.kind, ErrorKind::MissingPath | ErrorKind::NotFound) => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Dependency files count anywhere in the tree except installed or vendored copies.
const MAX_MANIFESTS: usize = 256;
const MAX_LISTED_PATHS: usize = 200_000;
const MAX_INVENTORY_PACKAGES: usize = 10_000;

fn is_manifest(path: &str) -> bool {
    let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
    (name == "package-lock.json" || super::dependencies::lockfiles::FILES.contains(&name))
        && !directory
            .split('/')
            .any(|part| matches!(part, "node_modules" | "vendor" | ".git"))
}

fn invalid(message: impl Into<String>) -> RepoError {
    RepoError::new(ErrorKind::Invalid, message.into())
}

async fn npm_packages(
    store: &super::store::RepoStore,
    sha: &str,
    directory: &str,
    path: &str,
    lock: &[u8],
) -> Result<Vec<Package>, RepoError> {
    let manifest_path = if directory.is_empty() {
        "package.json".to_owned()
    } else {
        format!("{directory}/package.json")
    };
    let manifest = file(store, sha, &manifest_path)
        .await?
        .ok_or_else(|| invalid("An npm lockfile needs its package manifest"))?;
    let lock = Lockfile::parse(&manifest, lock).map_err(|error| invalid(error.to_string()))?;
    if lock.unsupported > 0 {
        return Err(invalid("The npm dependency graph contains unsupported sources; submit a complete dependency snapshot"));
    }
    Ok(lock
        .packages
        .into_iter()
        .map(|package| Package {
            manifest: path.into(),
            ecosystem: "npm".into(),
            package_url: format!(
                "pkg:npm/{}@{}",
                package.name.replace('@', "%40"),
                package.version
            ),
            name: package.name,
            version: package.version.to_string(),
            scope: "unknown".into(),
        })
        .collect())
}

/// The detector and job that Codeyard's own lockfile scan saves its snapshot under.
const NATIVE_DETECTOR: &str = "Codeyard";
const NATIVE_CORRELATOR: &str = "lockfiles";

/// Whether a snapshot carries the native scan's identity, which submissions may not use.
pub(crate) fn is_native(snapshot: &crate::backend::dependency_graph::Snapshot) -> bool {
    snapshot.detector.name == NATIVE_DETECTOR && snapshot.job.correlator == NATIVE_CORRELATOR
}

/// The submitted snapshots of `reference` at `sha`, as their JSON documents.
async fn snapshot_values(
    state: &AppState,
    repo: &Repository,
    reference: &str,
    sha: &str,
) -> Result<Vec<serde_json::Value>, RepoError> {
    let branch = reference.starts_with("refs/heads/").then_some(reference);
    db::dependency_graph::snapshots(&state.pool, repo.id, branch, Some(sha))
        .await
        .map_err(graph_error)?
        .iter()
        .map(|snapshot| serde_json::to_value(snapshot).map_err(|error| unavailable(error.into())))
        .collect()
}

/// Every dependency file in the tree at `reference`. Any unreadable or unsupported file fails the
/// inventory, unless a submitted snapshot of the commit covers that manifest.
pub(crate) async fn native_inventory(
    state: &AppState,
    repo: &Repository,
    reference: &str,
) -> Result<(String, Vec<Package>), RepoError> {
    db::dependency_graph::check_enabled(&state.pool, repo.id).await.map_err(graph_error)?;
    let store = repo_store(repo, state.cluster.as_ref());
    let sha = store.commit(reference).await?.sha;
    let listing = store.file_paths(&sha, MAX_LISTED_PATHS).await?;
    // A partial listing could hide a lockfile, and a scan that saved it would mark its alerts fixed.
    if listing.truncated {
        return Err(invalid(
            "The repository has too many files to scan; submit a dependency snapshot",
        ));
    }
    let branch = reference.starts_with("refs/heads/").then_some(reference);
    // Codeyard's own earlier scan of this commit is not a submission, so it covers nothing. A
    // manifest is named by its source location, as `dependency_graph::packages` reads it.
    let covered: HashSet<String> =
        db::dependency_graph::snapshots(&state.pool, repo.id, branch, Some(&sha))
            .await
            .map_err(graph_error)?
            .iter()
            .filter(|snapshot| !is_native(snapshot))
            .flat_map(|snapshot| snapshot.manifests.values())
            .map(|manifest| {
                manifest
                    .file
                    .as_ref()
                    .and_then(|file| file.source_location.clone())
                    .unwrap_or_else(|| manifest.name.clone())
            })
            .collect();
    let manifests: Vec<&str> = listing
        .paths
        .iter()
        .map(String::as_str)
        .filter(|path| is_manifest(path))
        .collect();
    if manifests.len() > MAX_MANIFESTS {
        return Err(invalid(
            "The repository has too many dependency files; submit a dependency snapshot",
        ));
    }
    let mut packages = BTreeMap::new();
    for path in manifests {
        if covered.contains(path) {
            continue;
        }
        let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
        let Some(bytes) = file(&store, &sha, path).await? else {
            continue;
        };
        let parsed = if name == "package-lock.json" {
            npm_packages(&store, &sha, directory, path, &bytes).await?
        } else {
            super::dependencies::lockfiles::parse(path, &bytes)
                .map_err(|error| invalid(error.to_string()))?
        };
        for package in parsed {
            packages.insert(
                (package.manifest.clone(), package.package_url.clone()),
                package,
            );
            // Stop at the first excess package rather than collecting every manifest first.
            if packages.len() > MAX_INVENTORY_PACKAGES {
                return Err(invalid("Dependency inventory exceeds its limit"));
            }
        }
    }
    db::dependency_graph::check_enabled(&state.pool, repo.id).await.map_err(graph_error)?;
    Ok((sha, packages.into_values().collect()))
}

pub async fn inventory(
    state: &AppState,
    repo: &Repository,
    reference: &str,
) -> Result<(String, Vec<Package>), RepoError> {
    let (sha, native) = native_inventory(state, repo, reference).await?;
    let mut packages: BTreeMap<_, _> = native
        .into_iter()
        .map(|package| {
            (
                (package.manifest.clone(), package.package_url.clone()),
                package,
            )
        })
        .collect();
    for value in snapshot_values(state, repo, reference, &sha).await? {
        if let Some(manifests) = value["manifests"].as_object() {
            for (key, manifest) in manifests {
                // The file a manifest names, as the snapshot projection reads it: its source
                // location, then its name. The map key is only a label.
                let path = manifest["file"]["source_location"]
                    .as_str()
                    .or_else(|| manifest["name"].as_str())
                    .unwrap_or(key)
                    .to_owned();
                if let Some(resolved) = manifest["resolved"].as_object() {
                    for dependency in resolved.values() {
                        let Some(purl) = dependency["package_url"].as_str() else {
                            continue;
                        };
                        let (ecosystem, name, version) =
                            db::dependency_inventory::package_parts(purl).ok_or_else(|| {
                                unavailable(anyhow::anyhow!("Invalid stored package URL"))
                            })?;
                        let package = Package {
                            manifest: path.clone(),
                            ecosystem: ecosystem.into(),
                            name: crate::webui::decode_component(name),
                            version: crate::webui::decode_component(version),
                            package_url: purl.into(),
                            scope: dependency["scope"].as_str().unwrap_or("unknown").into(),
                        };
                        packages
                            .entry((path.clone(), purl.into()))
                            .and_modify(|known: &mut Package| {
                                if package.scope == "runtime" || known.scope == "unknown" {
                                    known.scope = package.scope.clone();
                                }
                            })
                            .or_insert(package);
                    }
                }
            }
        }
    }
    if packages.len() > 10000 {
        return Err(RepoError::new(
            ErrorKind::Invalid,
            "Dependency inventory exceeds its limit",
        ));
    }
    db::dependency_graph::check_enabled(&state.pool, repo.id).await.map_err(graph_error)?;
    Ok((sha, packages.into_values().collect()))
}

pub fn snapshot(
    sha: String,
    reference: String,
    packages: Vec<Package>,
) -> crate::backend::dependency_graph::Snapshot {
    use crate::backend::dependency_graph::{
        Dependency, Detector, Job, Manifest, ManifestFile, Snapshot,
    };
    let mut manifests: BTreeMap<String, Manifest> = BTreeMap::new();
    for package in packages {
        let manifest = manifests
            .entry(package.manifest.clone())
            .or_insert_with(|| Manifest {
                name: package.manifest.clone(),
                file: Some(ManifestFile {
                    source_location: Some(package.manifest),
                }),
                metadata: None,
                resolved: BTreeMap::new(),
            });
        manifest.resolved.insert(
            package.package_url.clone(),
            Dependency {
                package_url: Some(package.package_url),
                metadata: None,
                relationship: None,
                scope: (package.scope != "unknown").then_some(package.scope),
                dependencies: Vec::new(),
            },
        );
    }
    Snapshot {
        version: 0,
        job: Job {
            id: sha.clone(),
            correlator: NATIVE_CORRELATOR.into(),
            html_url: None,
        },
        sha,
        reference,
        detector: Detector {
            name: NATIVE_DETECTOR.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            url: env!("CARGO_PKG_REPOSITORY").into(),
        },
        scanned: chrono::Utc::now(),
        metadata: None,
        manifests,
    }
}

pub fn sbom(owner: &str, name: &str, sha: &str, packages: &[Package]) -> Value {
    let namespace = format!(
        "{}/dependency-graph/sbom/{}",
        crate::backend::api::github::repo_url(owner, name),
        uuid::Uuid::new_v4()
    );
    let packages: Vec<_> = packages.iter().enumerate().map(|(index, package)| json!({
        "SPDXID":format!("SPDXRef-Package-{index}"),"name":package.name,"versionInfo":package.version,
        "downloadLocation":"NOASSERTION","filesAnalyzed":false,"licenseConcluded":"NOASSERTION","licenseDeclared":"NOASSERTION",
        "externalRefs":[{"referenceCategory":"PACKAGE-MANAGER","referenceType":"purl","referenceLocator":package.package_url}]
    })).collect();
    let relationships:Vec<_> = packages.iter().map(|package|json!({"relationshipType":"DESCRIBES","spdxElementId":"SPDXRef-DOCUMENT","relatedSpdxElement":package["SPDXID"]})).collect();
    json!({"sbom":{"SPDXID":"SPDXRef-DOCUMENT","spdxVersion":"SPDX-2.3","dataLicense":"CC0-1.0","creationInfo":{"created":chrono::Utc::now().to_rfc3339(),"creators":["Tool: Codeyard"]},"name":format!("{owner}/{name}"),"documentNamespace":namespace,"comment":format!("Dependency inventory at commit {sha}"),"packages":packages,"relationships":relationships}})
}

//! Dependency inventories pin Git before reading either file and never execute package managers.
use crate::services::RepositoryServices;
use codeyard_database::models::Repository;
use codeyard_db_security::dependency_graph::Reader;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git_storage::repositories::store::repo_store;
use codeyard_repos::dependencies::npm::Lockfile;
use codeyard_types::repositories::Package;
use std::collections::BTreeMap;
use std::collections::HashSet;

fn unavailable(_: anyhow::Error) -> RepoError {
    RepoError::new(
        ErrorKind::Unavailable,
        "Dependency graph storage unavailable",
    )
}

fn graph_error(error: codeyard_types::dependency_graph::GraphError) -> RepoError {
    match error {
        error @ (codeyard_types::dependency_graph::GraphError::Denied
        | codeyard_types::dependency_graph::GraphError::Disabled
        | codeyard_types::dependency_graph::GraphError::Limit) => invalid(error.to_string()),
        error => unavailable(error.into()),
    }
}

async fn file(
    store: &codeyard_git_storage::repositories::store::RepoStore,
    sha: &str,
    name: &str,
) -> Result<Option<Vec<u8>>, RepoError> {
    let limit = codeyard_repos::dependencies::npm::MAX_LOCK_BYTES as u64;
    match store
        .blob_range(
            sha,
            name,
            codeyard_storage::range::ByteRange::Window {
                offset: 0,
                length: limit,
            },
        )
        .await
    {
        // An oversized file is refused from its size, so the whole blob is never parsed.
        Ok(blob) if !matches!(blob.mode, 0o100644 | 0o100755) => Err(invalid("Dependency files must be regular files; submit a dependency snapshot for symlinked manifests")),
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
    (matches!(
        name,
        "package.json" | "package-lock.json" | "npm-shrinkwrap.json"
    ) || codeyard_repos::dependencies::lockfiles::FILES.contains(&name))
        && !directory
            .split('/')
            .any(|part| matches!(part, "node_modules" | "vendor" | ".git"))
}

fn invalid(message: impl Into<String>) -> RepoError {
    RepoError::new(ErrorKind::Invalid, message.into())
}

async fn npm_packages(
    store: &codeyard_git_storage::repositories::store::RepoStore,
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
pub fn is_native(snapshot: &codeyard_types::dependency_graph::Snapshot) -> bool {
    snapshot.detector.name == NATIVE_DETECTOR && snapshot.job.correlator == NATIVE_CORRELATOR
}

/// The submitted snapshots of `reference` at `sha`, as their JSON documents.
async fn snapshot_values(
    state: &dyn RepositoryServices,
    reader: &Reader,
    repo: &Repository,
    reference: &str,
    sha: &str,
) -> Result<Vec<serde_json::Value>, RepoError> {
    let branch = reference.starts_with("refs/heads/").then_some(reference);
    codeyard_db_security::dependency_graph::snapshots(
        state.pool(),
        reader,
        repo.id,
        branch,
        Some(sha),
    )
    .await
    .map_err(graph_error)?
    .iter()
    .map(|snapshot| serde_json::to_value(snapshot).map_err(|error| unavailable(error.into())))
    .collect()
}

/// Every dependency file in the tree at `reference`. Any unreadable or unsupported file fails the
/// inventory, unless a submitted snapshot of the commit covers that manifest.
pub async fn native_inventory(
    state: &dyn RepositoryServices,
    reader: &Reader,
    repo: &Repository,
    reference: &str,
) -> Result<(String, Vec<Package>), RepoError> {
    codeyard_db_security::dependency_graph::check_read(state.pool(), reader, repo.id)
        .await
        .map_err(graph_error)?;
    let store = repo_store(repo, state.cluster());
    let sha = if codeyard_git::is_full_commit_id(reference) {
        store.commit(reference).await?
    } else {
        store.commit_from_primary(reference).await?
    }
    .sha;
    native_inventory_at(state, reader, repo, reference, &sha).await
}

/// The same scan of an already pinned commit. `reference` still decides which submitted
/// snapshots cover a manifest, so a branch that shares the commit cannot hide another's files.
pub(crate) async fn native_inventory_at(
    state: &dyn RepositoryServices,
    reader: &Reader,
    repo: &Repository,
    reference: &str,
    sha: &str,
) -> Result<(String, Vec<Package>), RepoError> {
    let store = repo_store(repo, state.cluster());
    let sha = sha.to_owned();
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
    let covered: HashSet<String> = codeyard_db_security::dependency_graph::snapshots(
        state.pool(),
        reader,
        repo.id,
        branch,
        Some(&sha),
    )
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
    let paths = listing
        .paths
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let sibling = |directory: &str, name: &str| {
        if directory.is_empty() {
            name.to_owned()
        } else {
            format!("{directory}/{name}")
        }
    };
    let mut packages = BTreeMap::new();
    for path in manifests {
        if covered.contains(path) {
            continue;
        }
        let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
        let shrinkwrap = sibling(directory, "npm-shrinkwrap.json");
        if name == "package-lock.json" && paths.contains(shrinkwrap.as_str()) {
            continue;
        }
        if name == "package.json" {
            if paths.contains(shrinkwrap.as_str())
                || paths.contains(sibling(directory, "package-lock.json").as_str())
            {
                continue;
            }
            return Err(invalid(
                "An npm manifest needs a supported lockfile; submit a complete dependency snapshot",
            ));
        }
        let Some(bytes) = file(&store, &sha, path).await? else {
            continue;
        };
        let parsed = if matches!(name, "package-lock.json" | "npm-shrinkwrap.json") {
            npm_packages(&store, &sha, directory, path, &bytes).await?
        } else {
            codeyard_repos::dependencies::lockfiles::parse(path, &bytes)
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
    codeyard_db_security::dependency_graph::check_read(state.pool(), reader, repo.id)
        .await
        .map_err(graph_error)?;
    Ok((sha, packages.into_values().collect()))
}

pub async fn inventory(
    state: &dyn RepositoryServices,
    reader: &Reader,
    repo: &Repository,
    reference: &str,
) -> Result<(String, Vec<Package>), RepoError> {
    let (sha, native) = native_inventory(state, reader, repo, reference).await?;
    let mut packages: BTreeMap<_, _> = native
        .into_iter()
        .map(|package| {
            (
                (package.manifest.clone(), package.package_url.clone()),
                package,
            )
        })
        .collect();
    for value in snapshot_values(state, reader, repo, reference, &sha).await? {
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
                            codeyard_types::dependency_inventory::package_parts(purl).ok_or_else(
                                || unavailable(anyhow::anyhow!("Invalid stored package URL")),
                            )?;
                        let package = Package {
                            manifest: path.clone(),
                            ecosystem: ecosystem.into(),
                            name: codeyard_types::paths::decode_component(name),
                            version: codeyard_types::paths::decode_component(version),
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
    codeyard_db_security::dependency_graph::check_read(state.pool(), reader, repo.id)
        .await
        .map_err(graph_error)?;
    Ok((sha, packages.into_values().collect()))
}

pub fn snapshot(
    sha: String,
    reference: String,
    packages: Vec<Package>,
) -> codeyard_types::dependency_graph::Snapshot {
    use codeyard_types::dependency_graph::Dependency;
    use codeyard_types::dependency_graph::Detector;
    use codeyard_types::dependency_graph::Job;
    use codeyard_types::dependency_graph::Manifest;
    use codeyard_types::dependency_graph::ManifestFile;
    use codeyard_types::dependency_graph::Snapshot;
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

/// The default branch's submitted snapshots and Codeyard's own scan, pinned to one commit.
/// A repository without commits has none.
pub async fn pinned_snapshots(
    state: &dyn RepositoryServices,
    reader: &Reader,
    repo: &Repository,
    reference: &str,
) -> Result<Vec<codeyard_types::dependency_graph::Snapshot>, RepoError> {
    let sha = match repo_store(repo, state.cluster())
        .commit_from_primary(reference)
        .await
    {
        Ok(commit) => commit.sha,
        Err(error) if matches!(error.kind, ErrorKind::NotFound | ErrorKind::MissingPath) => {
            return Ok(Vec::new())
        }
        Err(error) => return Err(error),
    };
    let mut snapshots = codeyard_db_security::dependency_graph::snapshots(
        state.pool(),
        reader,
        repo.id,
        Some(reference),
        Some(&sha),
    )
    .await
    .map_err(graph_error)?;
    append_native_snapshot(state, reader, repo, reference, Some(&sha), &mut snapshots).await?;
    Ok(snapshots)
}

/// Adds Codeyard's own scan to the submitted snapshots already chosen for the same reference and
/// commit, so every export shares one typed projection. A repository without commits has none.
pub async fn append_native_snapshot(
    state: &dyn RepositoryServices,
    reader: &Reader,
    repo: &Repository,
    reference: &str,
    pinned: Option<&str>,
    snapshots: &mut Vec<codeyard_types::dependency_graph::Snapshot>,
) -> Result<(), RepoError> {
    let scanned = match pinned {
        Some(sha) => native_inventory_at(state, reader, repo, reference, sha).await,
        None => native_inventory(state, reader, repo, reference).await,
    };
    match scanned {
        Ok((sha, packages)) => snapshots.push(snapshot(sha, reference.to_owned(), packages)),
        Err(error) if matches!(error.kind, ErrorKind::NotFound | ErrorKind::MissingPath) => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

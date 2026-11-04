//! Bounded npm lockfiles and conservative registry patch updates, without running npm.
use crate::npm::dependency_name;
use anyhow::ensure;
use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use codeyard_db_repos::repositories::dependencies::MAX_PACKAGES;
use semver::Version;
use semver::VersionReq;
use serde_json::Value;
use sha2::Sha512;
use std::collections::BTreeMap;

pub const MAX_LOCK_BYTES: usize = 4 * 1024 * 1024;

pub const MAX_MANIFEST: usize = 256 * 1024;
pub const MAX_ARCHIVE: usize = 8 * 1024 * 1024;
const MAX_EXPANDED: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Package {
    pub path: String,
    pub name: String,
    pub version: Version,
    pub resolved: String,
    pub integrity: String,
    location: Vec<String>,
}
pub struct Lockfile {
    manifest: Value,
    document: Value,
    version: u64,
    pub packages: Vec<Package>,
    pub unsupported: usize,
}
pub struct Patch {
    pub manifest: Vec<u8>,
    pub lock: Vec<u8>,
}
pub struct Archive {
    document: Value,
    pub version: Version,
    pub name: String,
    pub url: String,
    pub integrity: String,
}

pub fn registry_url(raw: &str) -> bool {
    if raw.len() > 4096 {
        return false;
    }
    url::Url::parse(raw).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("registry.npmjs.org")
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && url.query().is_none()
            && url.fragment().is_none()
    })
}
fn json(bytes: &[u8], limit: usize) -> Result<Value> {
    ensure!(bytes.len() <= limit, "Dependency file is too large");
    let value: Value = serde_json::from_slice(bytes)?;
    ensure!(value.is_object(), "Dependency file must be a JSON object");
    Ok(value)
}
fn formatted(value: &Value, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0; limit];
    let length = {
        let mut writer = std::io::Cursor::new(bytes.as_mut_slice());
        serde_json::to_writer_pretty(&mut writer, value)
            .context("Updated dependency file exceeds its size limit")?;
        writer.write_all(b"\n")?;
        writer.position() as usize
    };
    bytes.truncate(length);
    Ok(bytes)
}
fn package(path: String, location: Vec<String>, value: &Value) -> Option<Package> {
    if value.get("link").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let inferred = path.rsplit_once("node_modules/")?.1;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(inferred);
    if !dependency_name(name) {
        return None;
    }
    let raw_version = value.get("version")?.as_str()?;
    if raw_version.len() > 256 {
        return None;
    }
    let version = Version::parse(raw_version).ok()?;
    let resolved = value.get("resolved")?.as_str()?;
    if !registry_url(resolved) {
        return None;
    }
    Some(Package {
        name: name.into(),
        path,
        version,
        resolved: resolved.into(),
        integrity: value
            .get("integrity")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
        location,
    })
}
impl Lockfile {
    pub fn parse(manifest: &[u8], lock: &[u8]) -> Result<Self> {
        let manifest = json(manifest, MAX_MANIFEST)?;
        let document = json(lock, MAX_LOCK_BYTES)?;
        let version = document
            .get("lockfileVersion")
            .and_then(Value::as_u64)
            .context("Missing npm lockfileVersion")?;
        ensure!(
            (1..=3).contains(&version),
            "Unsupported npm lockfile version"
        );
        let mut packages = Vec::new();
        let mut unsupported = 0;
        let mut visited = 0;
        if version >= 2 {
            let entries = document
                .get("packages")
                .and_then(Value::as_object)
                .context("Missing npm packages")?;
            ensure!(entries.len() <= MAX_PACKAGES + 1, "Too many npm packages");
            let root = entries
                .get("")
                .and_then(Value::as_object)
                .context("Missing npm root package")?;
            for section in ["dependencies", "devDependencies", "optionalDependencies"] {
                let declarations =
                    |value: Option<&Value>| -> Result<serde_json::Map<String, Value>> {
                        match value {
                            None => Ok(serde_json::Map::new()),
                            Some(value) => value
                                .as_object()
                                .cloned()
                                .context("Invalid root dependency declarations"),
                        }
                    };
                ensure!(
                    declarations(manifest.get(section))? == declarations(root.get(section))?,
                    "The npm manifest and root lock declarations disagree"
                );
            }
            for (path, value) in entries {
                if path.is_empty() {
                    continue;
                }
                ensure!(value.is_object(), "Invalid npm package entry");
                ensure!(
                    path.len() <= 2048
                        && !path
                            .split('/')
                            .any(|p| p == ".." || p == "." || p.is_empty()),
                    "Invalid npm package path"
                );
                if !path.starts_with("node_modules/") {
                    unsupported += 1;
                    continue;
                }
                match package(path.clone(), vec!["packages".into(), path.clone()], value) {
                    Some(p) => packages.push(p),
                    None => unsupported += 1,
                }
            }
        } else {
            let entries = document
                .get("dependencies")
                .and_then(Value::as_object)
                .context("Missing npm dependencies")?;
            let mut pending: Vec<_> = entries
                .iter()
                .map(|(name, value)| {
                    (
                        format!("node_modules/{name}"),
                        vec!["dependencies".to_owned(), name.clone()],
                        value,
                        0usize,
                    )
                })
                .collect();
            while let Some((path, location, value, depth)) = pending.pop() {
                visited += 1;
                ensure!(
                    visited <= MAX_PACKAGES && depth <= 32 && path.len() <= 2048,
                    "Npm dependency graph is too large"
                );
                ensure!(value.is_object(), "Invalid npm dependency entry");
                if let Some(children) = value.get("dependencies") {
                    let children = children
                        .as_object()
                        .context("Invalid nested npm dependencies")?;
                    for (name, child) in children {
                        ensure!(dependency_name(name), "Invalid npm dependency name");
                        let mut nested = location.clone();
                        nested.extend(["dependencies".into(), name.clone()]);
                        pending.push((
                            format!("{path}/node_modules/{name}"),
                            nested,
                            child,
                            depth + 1,
                        ));
                    }
                }
                match package(path, location, value) {
                    Some(p) => packages.push(p),
                    None => unsupported += 1,
                }
            }
        }
        // A truncated/stale lock must not turn still-declared dependencies into
        // a successful empty inventory, which would incorrectly resolve old alerts.
        let mut declared = 0;
        for section in ["dependencies", "devDependencies", "optionalDependencies"] {
            if let Some(values) = manifest.get(section) {
                let values = values
                    .as_object()
                    .context("Invalid root dependency declarations")?;
                for (name, requirement) in values {
                    declared += 1;
                    ensure!(
                        declared <= MAX_PACKAGES
                            && dependency_name(name)
                            && requirement.as_str().is_some_and(
                                |s| s.len() <= 4096 && !s.chars().any(char::is_control)
                            ),
                        "Invalid or excessive root dependency declarations"
                    );
                    let present = if version >= 2 {
                        let installed_path = format!("node_modules/{name}");
                        document["packages"].get(installed_path.as_str()).is_some()
                    } else {
                        document["dependencies"].get(name).is_some()
                    };
                    if !present {
                        unsupported += 1;
                    }
                }
            }
        }
        if manifest.get("workspaces").is_some() {
            unsupported += 1;
        }
        ensure!(
            unsupported <= MAX_PACKAGES,
            "Too many unsupported dependency sources"
        );
        packages.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Self {
            manifest,
            document,
            version,
            packages,
            unsupported,
        })
    }
    pub fn queries(&self) -> Vec<(String, String)> {
        self.packages
            .iter()
            .map(|p| ((p.name.clone(), p.version.to_string()), ()))
            .collect::<BTreeMap<_, _>>()
            .into_keys()
            .collect()
    }
    pub fn patch(mut self, path: &str, old: &Archive, new: &Archive) -> Result<Patch> {
        ensure!(
            self.version >= 2,
            "Npm v1 updates require manual peer-constraint verification"
        );
        ensure!(
            self.unsupported == 0
                && self.manifest.get("workspaces").is_none()
                && self.manifest.get("overrides").is_none(),
            "Incomplete, workspace or overridden dependency graphs require manual review"
        );
        let package = self
            .packages
            .iter()
            .find(|p| p.path == path)
            .context("Dependency moved since the scan")?;
        ensure!(
            package.path == format!("node_modules/{}", package.name),
            "Transitive and aliased updates require manual review"
        );
        ensure!(
            old.name == package.name
                && old.version == package.version
                && old.integrity == package.integrity
                && old.url == package.resolved,
            "Locked package differs from the verified archive"
        );
        ensure!(
            new.name == old.name
                && old.version.major > 0
                && new.version.major == old.version.major
                && new.version.minor == old.version.minor
                && new.version.patch > old.version.patch
                && old.version.pre.is_empty()
                && new.version.pre.is_empty()
                && old.version.build.is_empty()
                && new.version.build.is_empty(),
            "Only stable patch updates are automated"
        );
        ensure!(
            graph(&old.document) == graph(&new.document),
            "The package dependency graph, platform or license changed"
        );
        // Do not silently invalidate another installed package's dependency or
        // peer constraint. Unknown/complex npm range syntax stays manual rather
        // than implementing a second package-manager resolver.
        let fields = ["dependencies", "optionalDependencies", "peerDependencies"];
        for installed in &self.packages {
            let mut entry = &self.document;
            for component in &installed.location {
                entry = entry.get(component).context("Lockfile entry moved")?;
            }
            for field in fields {
                if let Some(requirements) = entry.get(field) {
                    let requirements = requirements
                        .as_object()
                        .context("Invalid installed dependency requirements")?;
                    if let Some(requirement) = requirements.get(&package.name) {
                        let requirement = requirement
                            .as_str()
                            .context("Complex dependent requirements require manual review")?;
                        let compatible = if let Ok(exact) = Version::parse(requirement) {
                            exact == old.version && exact == new.version
                        } else {
                            VersionReq::parse(requirement)
                                .is_ok_and(|r| r.matches(&old.version) && r.matches(&new.version))
                        };
                        ensure!(
                            compatible,
                            "An installed dependency or peer requirement would be invalidated"
                        );
                    }
                }
            }
        }
        let mut declarations = Vec::new();
        for section in ["dependencies", "devDependencies", "optionalDependencies"] {
            if let Some(requirement) = self
                .manifest
                .get(section)
                .and_then(|v| v.get(&package.name))
            {
                declarations.push((
                    section,
                    requirement
                        .as_str()
                        .context("Invalid dependency requirement")?
                        .to_owned(),
                ));
            }
        }
        ensure!(
            declarations.len() == 1,
            "A single direct package declaration is required"
        );
        let (section, requirement) = declarations.remove(0);
        let operator = if requirement.starts_with(['^', '~', '=']) {
            &requirement[..1]
        } else {
            ""
        };
        let declared = Version::parse(&requirement[operator.len()..])
            .context("Complex npm requirements require manual review")?;
        let matches = if operator.is_empty() || operator == "=" {
            declared == old.version
        } else {
            VersionReq::parse(&requirement)?.matches(&old.version)
        };
        ensure!(
            declared.pre.is_empty() && declared.build.is_empty() && matches,
            "The manifest and lockfile disagree"
        );
        let new_requirement = format!("{operator}{}", new.version);
        self.manifest[section][&package.name] = Value::String(new_requirement.clone());
        let mut entry = &mut self.document;
        for component in &package.location {
            entry = entry.get_mut(component).context("Lockfile entry moved")?;
        }
        {
            let actual = graph(entry);
            let expected = graph(&old.document);
            ensure!(
                [
                    "dependencies",
                    "optionalDependencies",
                    "peerDependencies",
                    "peerDependenciesMeta"
                ]
                .iter()
                .all(|key| actual[*key] == expected[*key]),
                "The locked dependency graph differs from the verified archive"
            );
        }
        replace(entry, new)?;
        if self.version >= 2 {
            let root = self
                .document
                .get_mut("packages")
                .and_then(|p| p.get_mut(""))
                .context("Missing npm root package")?;
            ensure!(
                root.get(section)
                    .and_then(|v| v.get(&package.name))
                    .and_then(Value::as_str)
                    == Some(requirement.as_str()),
                "The npm root declaration is inconsistent"
            );
            root[section][&package.name] = Value::String(new_requirement);
        }
        if self.version == 2 {
            let mirror = self
                .document
                .get_mut("dependencies")
                .and_then(|p| p.get_mut(&package.name))
                .context("Missing npm v2 dependency mirror")?;
            ensure!(
                mirror.get("version").and_then(Value::as_str)
                    == Some(old.version.to_string().as_str()),
                "The npm v2 mirror is inconsistent"
            );
            ensure!(
                mirror.get("resolved").and_then(Value::as_str) == Some(old.url.as_str())
                    && mirror.get("integrity").and_then(Value::as_str)
                        == Some(old.integrity.as_str()),
                "The npm v2 mirror archive is inconsistent"
            );
            legacy_graph(mirror, &old.document)?;
            replace(mirror, new)?;
        }
        Ok(Patch {
            manifest: formatted(&self.manifest, MAX_MANIFEST)?,
            lock: formatted(&self.document, MAX_LOCK_BYTES)?,
        })
    }
}
fn legacy_graph(entry: &Value, document: &Value) -> Result<()> {
    let mut expected = serde_json::Map::new();
    for key in ["dependencies", "optionalDependencies"] {
        if let Some(value) = document.get(key) {
            let map = value
                .as_object()
                .context("Invalid npm package dependencies")?;
            for (name, requirement) in map {
                ensure!(
                    dependency_name(name) && requirement.as_str().is_some_and(|s| s.len() <= 2048),
                    "Invalid npm dependency requirement"
                );
                expected.insert(name.clone(), requirement.clone());
            }
        }
    }
    let actual = match entry.get("requires") {
        None => serde_json::Map::new(),
        Some(value) => value
            .as_object()
            .context("Invalid locked npm requirements")?
            .clone(),
    };
    ensure!(
        actual == expected,
        "The locked requirements differ from the verified archive"
    );
    Ok(())
}
fn replace(entry: &mut Value, new: &Archive) -> Result<()> {
    ensure!(entry.is_object(), "Invalid npm package entry");
    entry["version"] = Value::String(new.version.to_string());
    entry["resolved"] = Value::String(new.url.clone());
    entry["integrity"] = Value::String(new.integrity.clone());
    Ok(())
}
fn graph(value: &Value) -> Value {
    let mut graph = serde_json::Map::new();
    for key in [
        "dependencies",
        "optionalDependencies",
        "peerDependencies",
        "peerDependenciesMeta",
        "bundleDependencies",
        "bundledDependencies",
        "engines",
        "os",
        "cpu",
        "libc",
        "license",
        "bin",
        "directories",
    ] {
        let field = value.get(key).cloned().unwrap_or(Value::Null);
        let empty = field.as_object().is_some_and(|v| v.is_empty())
            || field.as_array().is_some_and(|v| v.is_empty());
        graph.insert(key.into(), if empty { Value::Null } else { field });
    }
    Value::Object(graph)
}
fn sri(raw: &str) -> Result<Vec<u8>> {
    let bytes = STANDARD.decode(
        raw.strip_prefix("sha512-")
            .context("Only SHA-512 npm archives are automated")?,
    )?;
    ensure!(bytes.len() == 64, "Invalid npm archive integrity");
    Ok(bytes)
}
impl Archive {
    pub fn verify(metadata: &Value, compressed: &[u8], expected: Option<&str>) -> Result<Self> {
        ensure!(compressed.len() <= MAX_ARCHIVE, "Npm archive is too large");
        let url = metadata
            .pointer("/dist/tarball")
            .and_then(Value::as_str)
            .context("Missing npm archive URL")?;
        ensure!(registry_url(url), "Untrusted npm archive URL");
        let integrity = metadata
            .pointer("/dist/integrity")
            .and_then(Value::as_str)
            .context("Missing npm archive integrity")?;
        let digest = sri(integrity)?;
        ensure!(
            digest.as_slice() == &Sha512::digest(compressed)[..],
            "Npm archive checksum mismatch"
        );
        if let Some(expected) = expected {
            ensure!(
                sri(expected)? == digest,
                "Locked npm archive checksum mismatch"
            );
        }
        ensure!(
            metadata
                .get("deprecated")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty(),
            "Deprecated npm versions require manual review"
        );
        let decoder = flate2::read::MultiGzDecoder::new(compressed).take(MAX_EXPANDED + 1);
        let mut archive = tar::Archive::new(decoder);
        let mut document = None;
        for (index, entry) in archive.entries()?.enumerate() {
            ensure!(index < 10000, "Too many npm archive entries");
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            ensure!(
                crate::repositories::imports::archive::safe_path(&path)
                    && path.starts_with("package"),
                "Unsafe npm archive path"
            );
            ensure!(
                path != std::path::Path::new("package/binding.gyp")
                    && !path.components().any(|p| p.as_os_str() == "node_modules"),
                "Bundled or native-build npm packages require manual review"
            );
            ensure!(
                entry.header().entry_type().is_file() || entry.header().entry_type().is_dir(),
                "Npm archives with links or special files require manual review"
            );
            if path == std::path::Path::new("package/package.json") {
                ensure!(
                    document.is_none()
                        && entry.header().entry_type().is_file()
                        && entry.size() <= MAX_MANIFEST as u64,
                    "Invalid npm package manifest"
                );
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                document = Some(json(&bytes, MAX_MANIFEST)?);
            }
        }
        let mut decoder = archive.into_inner();
        let mut tail = [0; 4096];
        loop {
            let n = decoder.read(&mut tail)?;
            if n == 0 {
                break;
            }
            ensure!(
                tail[..n].iter().all(|b| *b == 0),
                "Unexpected data after the npm archive"
            );
        }
        ensure!(decoder.limit() > 0, "Expanded npm archive is too large");
        let document = document.context("Npm archive has no package manifest")?;
        let name = document
            .get("name")
            .and_then(Value::as_str)
            .context("Missing npm package name")?;
        ensure!(
            dependency_name(name) && metadata.get("name").and_then(Value::as_str) == Some(name),
            "Npm package name mismatch"
        );
        let version = Version::parse(
            document
                .get("version")
                .and_then(Value::as_str)
                .context("Missing npm package version")?,
        )?;
        ensure!(
            metadata.get("version").and_then(Value::as_str) == Some(version.to_string().as_str()),
            "Npm package version mismatch"
        );
        if let Some(scripts) = document.get("scripts") {
            let scripts = scripts.as_object().context("Invalid npm package scripts")?;
            ensure!(
                ["preinstall", "install", "postinstall", "prepare"]
                    .iter()
                    .all(|key| !scripts.contains_key(*key)),
                "Npm install scripts require manual review"
            );
        }
        ensure!(
            ["bundleDependencies", "bundledDependencies"]
                .iter()
                .all(|key| match document.get(*key) {
                    None | Some(Value::Null) | Some(Value::Bool(false)) => true,
                    Some(Value::Array(values)) => values.is_empty(),
                    _ => false,
                }),
            "Bundled npm dependencies require manual review"
        );
        Ok(Self {
            name: name.into(),
            version,
            document,
            url: url.into(),
            integrity: integrity.into(),
        })
    }
}
#[cfg(test)]
mod tests;

//! Reads migration records and Git object files without extracting an upstream tar to disk.
pub mod git;
use anyhow::{ensure, Context, Result};
use serde::{
    de::{SeqAccess, Visitor},
    Deserialize, Deserializer,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    io::Read,
    path::{Component, Path},
};

const MAX_ARCHIVE_BYTES: usize = crate::backend::storage::MAX_OBJECT_BYTES as usize;
pub(crate) const MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024;
// The export writes archives within these limits too, so every archive it reports as complete
// can be imported.
pub(crate) const MAX_METADATA_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_RECORDS: usize = 50_000;
pub(crate) const MAX_ENTRIES: usize = 100_000;
const MAX_TAR_BYTES: u64 = MAX_UNPACKED_BYTES + MAX_ENTRIES as u64 * 2048 + 1024;

pub struct ArchiveSource {
    pub repository: Value,
    pub records: BTreeMap<String, Vec<Value>>,
    /// Relative to the selected bare repository. Configuration, hooks and index files are ignored.
    pub git_files: BTreeMap<String, Vec<u8>>,
    pub attachments: BTreeMap<String, Vec<u8>>,
}

struct Records(Vec<Value>);
impl<'de> Deserialize<'de> for Records {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct RecordVisitor;
        impl<'de> Visitor<'de> for RecordVisitor {
            type Value = Records;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded array of migration records")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> std::result::Result<Records, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<Value>()? {
                    if values.len() == MAX_RECORDS || !value.is_object() {
                        return Err(serde::de::Error::custom("Invalid migration records"));
                    }
                    values.push(value);
                }
                Ok(Records(values))
            }
        }
        deserializer.deserialize_seq(RecordVisitor)
    }
}

/// A nonempty relative path of plain names, without roots or traversal.
pub(crate) fn safe_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn record_kind(path: &str) -> Option<String> {
    if path.contains('/') {
        return None;
    }
    let stem = path.strip_suffix(".json")?;
    let plural = stem
        .rsplit_once('_')
        .filter(|(_, suffix)| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
        .map_or(stem, |(kind, _)| kind);
    Some(match plural {
        "repositories" => "repository".to_owned(),
        "team_repositories" => "team_repository".to_owned(),
        "schema" | "manifest" => return None,
        other => other.strip_suffix('s')?.to_owned(),
    })
}

fn canonical_url(value: &str) -> Option<String> {
    let mut url = url::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    Some(url.as_str().trim_end_matches('/').to_owned())
}

fn repository_urls(repository: &Value) -> Vec<String> {
    let mut roots = Vec::new();
    for field in ["url", "html_url"] {
        if let Some(root) = repository[field].as_str().and_then(canonical_url) {
            roots.push(root);
        }
    }
    if let Some(url) = repository["issues_url"]
        .as_str()
        .and_then(|url| url.split_once("/issues").map(|(root, _)| root))
        .and_then(canonical_url)
    {
        roots.push(url);
    }
    roots.sort_unstable();
    roots.dedup();
    roots
}

fn belongs_to(value: &Value, repository: &Value, roots: &[String]) -> bool {
    if value["repository"].is_object() && value["repository"]["id"] == repository["id"] {
        return true;
    }
    for field in [
        "repository",
        "repository_url",
        "url",
        "html_url",
        "issue_url",
        "pull_request_url",
        "pull_request",
        "release",
    ] {
        if let Some(url) = value[field].as_str().and_then(canonical_url) {
            if roots.iter().any(|root| {
                url == *root
                    || url
                        .strip_prefix(root)
                        .is_some_and(|rest| rest.starts_with('/'))
            }) {
                return true;
            }
        }
    }
    false
}

fn git_object_path(path: &str) -> bool {
    if path == "HEAD" || path == "packed-refs" || path.starts_with("refs/") {
        return true;
    }
    if let Some(name) = path
        .strip_prefix("objects/pack/pack-")
        .and_then(|name| name.strip_suffix(".pack"))
    {
        return crate::backend::git::is_full_commit_id(name);
    }
    if let Some((directory, file)) = path
        .strip_prefix("objects/")
        .and_then(|path| path.split_once('/'))
    {
        return directory.len() == 2
            && file.len() == 38
            && directory
                .bytes()
                .chain(file.bytes())
                .all(|byte| byte.is_ascii_hexdigit());
    }
    false
}

pub fn read(bytes: &[u8], selected: Option<&str>) -> Result<ArchiveSource> {
    ensure!(
        bytes.len() <= MAX_ARCHIVE_BYTES,
        "Migration archive exceeds its size limit"
    );
    let decoder = flate2::read::MultiGzDecoder::new(bytes).take(MAX_TAR_BYTES + 1);
    let mut archive = tar::Archive::new(decoder);
    archive.set_ignore_zeros(true);
    let mut files = BTreeMap::new();
    let mut records: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut metadata_bytes = 0;
    let mut record_count = 0;
    let mut size = 0;
    for (count, entry) in archive.entries()?.enumerate() {
        ensure!(count < MAX_ENTRIES, "Too many migration entries");
        let mut entry = entry?;
        let original_path = entry.path()?.into_owned();
        ensure!(
            original_path
                .components()
                .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)),
            "Invalid migration path"
        );
        let path: std::path::PathBuf = original_path
            .components()
            .filter_map(|part| {
                if let Component::Normal(name) = part {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        if path.as_os_str().is_empty() {
            ensure!(
                entry.header().entry_type().is_dir() && entry.size() == 0,
                "Invalid migration path"
            );
            continue;
        }
        let path = path.to_str().context("Invalid migration path")?.to_owned();
        ensure!(
            entry.header().entry_type().is_file() || entry.header().entry_type().is_dir(),
            "Migration links and special files are not supported"
        );
        ensure!(
            entry.size() <= MAX_UNPACKED_BYTES - size,
            "Migration archive exceeds its size limit"
        );
        size += entry.size();
        if entry.header().entry_type().is_dir() {
            ensure!(entry.size() == 0, "Invalid migration directory");
            continue;
        }
        ensure!(!files.contains_key(&path), "Duplicate migration entry");
        let kind = record_kind(&path);
        if kind.is_some() {
            ensure!(
                entry.size() <= (MAX_METADATA_BYTES - metadata_bytes) as u64,
                "Migration metadata exceeds its size limit"
            );
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        if let Some(kind) = kind {
            ensure!(
                data.len() <= MAX_METADATA_BYTES - metadata_bytes,
                "Migration metadata exceeds its size limit"
            );
            metadata_bytes += data.len();
            let values = serde_json::from_slice::<Records>(&data)?.0;
            ensure!(
                values.len() <= MAX_RECORDS - record_count,
                "Too many migration records"
            );
            record_count += values.len();
            records.entry(kind).or_default().extend(values);
        }
        files.insert(path, data);
    }
    // Finish the gzip stream as well: tar's zero blocks must not hide a bad CRC or huge padding.
    let mut decoder = archive.into_inner();
    std::io::copy(&mut decoder, &mut std::io::sink())?;
    ensure!(
        decoder.limit() > 0,
        "Migration archive exceeds its size limit"
    );
    let repositories = records
        .remove("repository")
        .context("Migration has no repositories")?;
    let repository = match selected {
        Some(name) => {
            let matches: Vec<_> = repositories
                .iter()
                .filter(|repository| {
                    repository["full_name"]
                        .as_str()
                        .is_some_and(|full_name| full_name.eq_ignore_ascii_case(name))
                })
                .collect();
            ensure!(matches.len() == 1, "Select exactly one source repository");
            matches[0].clone()
        }
        None => {
            ensure!(
                repositories.len() == 1,
                "Select a source repository from this archive"
            );
            repositories[0].clone()
        }
    };
    let name = repository["full_name"]
        .as_str()
        .context("Migration repository has no name")?;
    ensure!(
        safe_path(Path::new(name))
            && name.contains('/')
            && name
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "Invalid migration repository name"
    );
    let roots = repository_urls(&repository);
    ensure!(
        repositories.len() == 1 || !roots.is_empty(),
        "Migration repository has no identity URL"
    );
    for (kind, values) in &mut records {
        if !matches!(
            kind.as_str(),
            "user"
                | "organization"
                | "organization_membership"
                | "team"
                | "team_membership"
                | "team_repository"
                | "project"
                | "project_field"
                | "project_item"
                | "project_view"
        ) && repositories.len() > 1
        {
            values.retain(|value| belongs_to(value, &repository, &roots));
        }
    }
    let prefixes = [
        format!("repositories/{name}.git/"),
        format!("git/{name}.git/"),
        format!("repositories/{}.git/", repository["id"]),
        format!("git/{}.git/", repository["id"]),
    ];
    let selected_attachments: std::collections::HashSet<_> = records
        .values()
        .flatten()
        .filter_map(|record| record["archive_path"].as_str())
        .collect();
    let mut git_files = BTreeMap::new();
    let mut attachments = BTreeMap::new();
    let mut git_bytes = 0;
    for (path, data) in files {
        if path.starts_with("attachments/") {
            if repositories.len() == 1 || selected_attachments.contains(path.as_str()) {
                attachments.insert(path, data);
            }
            continue;
        }
        if let Some(relative) = prefixes.iter().find_map(|prefix| path.strip_prefix(prefix)) {
            if git_object_path(relative) {
                ensure!(
                    data.len() <= super::MAX_IMPORT_PACK_BYTES - git_bytes,
                    "Migration Git data exceeds its size limit"
                );
                git_bytes += data.len();
                ensure!(
                    git_files.insert(relative.to_owned(), data).is_none(),
                    "Duplicate migration Git entry"
                );
            }
        }
    }
    Ok(ArchiveSource {
        repository,
        records,
        git_files,
        attachments,
    })
}

/// Ref listings are data, never commands or paths to execute.
pub fn refs(source: &ArchiveSource) -> Result<(HashMap<String, String>, Option<String>)> {
    let mut refs = HashMap::new();
    if let Some(bytes) = source.git_files.get("packed-refs") {
        for line in std::str::from_utf8(bytes)?.lines() {
            if line.is_empty() || line.starts_with(['#', '^']) {
                continue;
            }
            let (sha, name) = line.split_once(' ').context("Invalid migration ref")?;
            ensure!(
                crate::backend::git::is_full_commit_id(sha)
                    && name.starts_with("refs/")
                    && git2::Reference::is_valid_name(name),
                "Invalid migration ref"
            );
            ensure!(
                refs.insert(name.to_owned(), sha.to_owned()).is_none(),
                "Duplicate migration ref"
            );
        }
    }
    for (path, bytes) in &source.git_files {
        if path.starts_with("refs/") {
            let sha = std::str::from_utf8(bytes)?.trim();
            ensure!(
                git2::Reference::is_valid_name(path) && crate::backend::git::is_full_commit_id(sha),
                "Invalid migration ref"
            );
            refs.insert(path.clone(), sha.to_owned());
        }
    }
    ensure!(
        refs.len() <= crate::backend::git::MAX_REPOSITORY_REFS,
        "Too many migration refs"
    );
    // A migration carries user history, not another server's publication attestations.
    // The importer creates fresh evidence under the importing actor's authority.
    refs.retain(|name, _| name != "refs/codeyard" && !name.starts_with("refs/codeyard/"));
    let head = source
        .git_files
        .get("HEAD")
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|head| head.trim().strip_prefix("ref: refs/heads/"))
        .map(str::to_owned);
    if let Some(head) = &head {
        ensure!(
            crate::backend::git::branch_ref(head).is_some(),
            "Invalid migration HEAD"
        );
    }
    Ok((refs, head))
}

#[cfg(test)]
mod tests;

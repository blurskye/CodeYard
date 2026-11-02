//! Migration records and bare repositories share one bounded tar.gz archive.
use crate::backend::repositories::imports::archive::{
    MAX_ENTRIES, MAX_METADATA_BYTES, MAX_RECORDS,
};
use anyhow::{ensure, Result};
use flate2::{write::GzEncoder, Compression};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    path::{Component, Path},
};

const MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024;

struct ArchiveBytes(Vec<u8>);
impl Write for ArchiveBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > crate::backend::storage::MAX_OBJECT_BYTES as usize - self.0.len() {
            return Err(io::Error::other("Migration archive exceeds its size limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct MigrationArchive {
    archive: tar::Builder<GzEncoder<ArchiveBytes>>,
    bytes: u64,
    entries: usize,
    records: usize,
    metadata: usize,
    people: BTreeMap<String, Value>,
    chunks: BTreeMap<String, usize>,
}

impl Default for MigrationArchive {
    fn default() -> Self {
        Self {
            archive: tar::Builder::new(GzEncoder::new(
                ArchiveBytes(Vec::new()),
                Compression::default(),
            )),
            bytes: 0,
            entries: 0,
            records: 0,
            metadata: 0,
            people: BTreeMap::new(),
            chunks: BTreeMap::new(),
        }
    }
}

impl MigrationArchive {
    fn check_path(path: &str) -> Result<()> {
        ensure!(
            !path.is_empty()
                && Path::new(path)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "Invalid migration entry"
        );
        Ok(())
    }

    fn count_entry(&mut self) -> Result<()> {
        self.entries += 1;
        ensure!(
            self.entries <= MAX_ENTRIES,
            "Migration archive has too many entries"
        );
        Ok(())
    }

    fn directory(&mut self, path: &str) -> Result<()> {
        Self::check_path(path)?;
        self.count_entry()?;
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        self.archive.append_data(&mut header, path, io::empty())?;
        Ok(())
    }

    pub fn append(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        Self::check_path(path)?;
        self.count_entry()?;
        ensure!(
            bytes.len() as u64 <= MAX_UNPACKED_BYTES - self.bytes,
            "Migration archive exceeds its size limit"
        );
        self.bytes += bytes.len() as u64;
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        self.archive.append_data(&mut header, path, bytes)?;
        Ok(())
    }

    pub fn repository(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.to_string_lossy().into_owned();
            let path = format!("repositories/{name}.git/{path}");
            if entry.header().entry_type().is_dir() {
                self.directory(&path)?;
                continue;
            }
            ensure!(
                entry.header().entry_type().is_file(),
                "Invalid migration Git entry"
            );
            ensure!(
                entry.size() <= MAX_UNPACKED_BYTES - self.bytes,
                "Migration archive exceeds its size limit"
            );
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            self.append(&path, &data)?;
        }
        Ok(())
    }

    pub fn person(&mut self, value: Value) {
        if let Some(url) = value["html_url"].as_str() {
            self.people.insert(url.to_owned(), value);
        }
    }

    fn collect_people(&mut self, value: &Value) {
        match value {
            Value::Object(object) => {
                if let (Some(login), Some(url)) =
                    (value["login"].as_str(), value["html_url"].as_str())
                {
                    if value["id"].is_i64() && !login.is_empty() {
                        self.people
                            .entry(url.to_owned())
                            .or_insert_with(|| value.clone());
                    }
                }
                for child in object.values() {
                    self.collect_people(child);
                }
            }
            Value::Array(values) => {
                for child in values {
                    self.collect_people(child);
                }
            }
            _ => {}
        }
    }

    pub fn records(&mut self, kind: &str, values: Vec<Value>) -> Result<()> {
        let chunk = self.chunks.entry(kind.to_owned()).or_default();
        *chunk += 1;
        let plural = match kind {
            "repository" => "repositories".to_owned(),
            "team_repository" => "team_repositories".to_owned(),
            _ => format!("{kind}s"),
        };
        let path = format!("{plural}_{chunk:06}.json");
        let mut records = Vec::new();
        for mut value in values {
            ensure!(value.is_object(), "Invalid migration record");
            self.collect_people(&value);
            value["type"] = Value::String(kind.to_owned());
            if let Some(url) = value["html_url"].as_str().map(str::to_owned) {
                value["url"] = Value::String(url);
            }
            records.push(value);
        }
        self.records += records.len();
        ensure!(
            self.records <= MAX_RECORDS,
            "Migration archive has too many records"
        );
        let json = serde_json::to_vec(&records)?;
        self.metadata += json.len();
        ensure!(
            self.metadata <= MAX_METADATA_BYTES,
            "Migration archive has too much metadata"
        );
        self.append(&path, &json)
    }

    pub fn finish(mut self) -> Result<Vec<u8>> {
        let people = std::mem::take(&mut self.people);
        let (organizations, users): (Vec<_>, Vec<_>) = people
            .into_values()
            .partition(|person| person["type"] == "Organization");
        self.records("user", users)?;
        self.records("organization", organizations)?;
        Ok(self.archive.into_inner()?.finish()?.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn migration_records_and_people() {
        let mut archive = MigrationArchive::default();
        let user = json!({"id":7,"login":"original","type":"User","html_url":"https://example.test/original"});
        archive
            .records("repository", vec![json!({"name":"project","owner":user})])
            .unwrap();
        archive
            .records("issue", vec![json!({"title":"Original issue","user":user})])
            .unwrap();
        let bytes = archive.finish().unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
        let mut records = BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            records.insert(path, serde_json::from_slice::<Value>(&bytes).unwrap());
        }
        assert_eq!(records["repositories_000001.json"][0]["type"], "repository");
        assert_eq!(records["issues_000001.json"][0]["title"], "Original issue");
        assert_eq!(records["users_000001.json"].as_array().unwrap().len(), 1);
        assert_eq!(records["users_000001.json"][0]["login"], "original");
    }

    #[test]
    fn migration_archive_preserves_empty_bare_repositories() {
        let directory = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(directory.path()).unwrap();
        let mut input = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
        for name in ["HEAD", "config"] {
            input
                .append_path_with_name(directory.path().join(name), name)
                .unwrap();
        }
        for name in ["objects", "refs"] {
            input
                .append_dir_all(name, directory.path().join(name))
                .unwrap();
        }
        let input = input.into_inner().unwrap().finish().unwrap();
        let mut archive = MigrationArchive::default();
        archive.repository("owner/empty", &input).unwrap();
        let bytes = archive.finish().unwrap();
        let output = tempfile::tempdir().unwrap();
        tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()))
            .unpack(output.path())
            .unwrap();
        let repository =
            git2::Repository::open_bare(output.path().join("repositories/owner/empty.git"))
                .unwrap();
        assert!(repository.is_empty().unwrap());
    }

    #[test]
    fn migration_archive_refuses_unsafe_or_oversized_entries() {
        let mut archive = MigrationArchive::default();
        for path in ["/absolute", "../outside", "objects/../../outside", ""] {
            assert!(archive.append(path, b"data").is_err());
        }
        assert!(archive.directory("../outside").is_err());
        assert!(archive.records("issue", vec![Value::Null]).is_err());
        archive.bytes = MAX_UNPACKED_BYTES;
        assert!(archive.append("overflow", b"data").is_err());
        let mut input = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o644);
        input.append_link(&mut header, "HEAD", "/outside").unwrap();
        let bytes = input.into_inner().unwrap().finish().unwrap();
        assert!(MigrationArchive::default()
            .repository("owner/project", &bytes)
            .is_err());
    }
}

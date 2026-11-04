//! Git database objects are written without publishing a reference.
use super::repository::process;
use super::repository::{MAX_COMMIT_EMAIL_BYTES, MAX_COMMIT_NAME_BYTES};
use crate::backend::git::error::{ErrorKind, RepoError};
use anyhow::{ensure, Context, Result};
use git2::{ObjectType, Oid, Repository};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// Makes libgit2 flush the objects and refs it writes before it returns, as
/// `core.fsync=all` does for the `git` processes. Call it once at startup.
pub fn flush_writes_to_disk() {
    libgit2_sys::init();
    // SAFETY: this option takes one int argument, and libgit2 is initialized above.
    let status = unsafe {
        libgit2_sys::git_libgit2_opts(
            libgit2_sys::GIT_OPT_ENABLE_FSYNC_GITDIR as libc::c_int,
            1 as libc::c_int,
        )
    };
    debug_assert!(status >= 0);
}

const MAX_ENTRIES: usize = 1_000;
const MAX_BYTES: usize = 1024 * 1024;
pub const MAX_BLOB_WRITE_BYTES: usize = 100 * 1024 * 1024;
const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub date: Option<chrono::DateTime<chrono::FixedOffset>>,
}

/// Admission for newly supplied identities. Legacy replay preserves already stored bytes.
pub(crate) fn validate_identity(name: &str, email: &str) -> std::result::Result<(), RepoError> {
    if name.trim().is_empty()
        || name.len() > MAX_COMMIT_NAME_BYTES
        || email.trim().is_empty()
        || email.len() > MAX_COMMIT_EMAIL_BYTES
        || [name, email]
            .iter()
            .any(|value| value.contains(['\0', '\n', '\r', '<', '>']))
    {
        return Err(invalid("Invalid Git identity"));
    }
    Ok(())
}

impl Signature {
    pub(crate) fn validate(&self) -> std::result::Result<(), RepoError> {
        validate_identity(&self.name, &self.email)?;
        // libgit2 writes object dates as unsigned 32-bit seconds, so other dates would wrap.
        if self
            .date
            .is_some_and(|date| u32::try_from(date.timestamp()).is_err())
        {
            return Err(invalid("Git dates must fall between 1970 and 2106"));
        }
        Ok(())
    }

    fn git(&self) -> Result<git2::Signature<'static>> {
        self.validate()?;
        let signature = match self.date {
            Some(date) => git2::Signature::new(
                &self.name,
                &self.email,
                &git2::Time::new(date.timestamp(), date.offset().local_minus_utc() / 60),
            ),
            None => git2::Signature::now(&self.name, &self.email),
        };
        signature.map_err(|error| {
            if error.class() == git2::ErrorClass::Invalid {
                invalid("Invalid Git identity").into()
            } else {
                error.into()
            }
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeChange {
    pub path: String,
    pub mode: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub sha: Option<String>,
    pub content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ObjectWrite {
    Blob,
    Tree {
        base_tree: Option<String>,
        tree: Vec<TreeChange>,
    },
    Commit {
        message: String,
        tree: String,
        parents: Vec<String>,
        author: Signature,
        committer: Signature,
        signature: Option<String>,
    },
    Tag {
        tag: String,
        message: String,
        object: String,
        kind: String,
        tagger: Signature,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeObject {
    pub sha: String,
    pub tree: Vec<TreeObjectEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeObjectEntry {
    pub path: String,
    #[serde(default)]
    pub path_raw: Vec<u8>,
    pub mode: String,
    pub kind: String,
    pub sha: String,
    pub size: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagObject {
    pub sha: String,
    pub tag: String,
    pub message: String,
    pub object: String,
    pub kind: String,
    pub tagger: Option<Signature>,
    #[serde(default)]
    pub signature: Option<String>,
    /// The exact signed bytes, which need not be UTF-8.
    #[serde(default)]
    pub signature_payload: Option<Vec<u8>>,
}

fn invalid(message: &'static str) -> RepoError {
    RepoError::new(ErrorKind::Invalid, message)
}

fn oid(value: &str) -> Result<Oid> {
    if !codeyard_support::git_ids::is_full_commit_id(value) {
        return Err(invalid("Expected a full object ID").into());
    }
    Oid::from_str(value).map_err(|_| invalid("Invalid object ID").into())
}

fn kind(value: &str) -> Result<ObjectType> {
    match value {
        "blob" => Ok(ObjectType::Blob),
        "tree" => Ok(ObjectType::Tree),
        "commit" => Ok(ObjectType::Commit),
        "tag" => Ok(ObjectType::Tag),
        _ => Err(invalid("Invalid object type").into()),
    }
}

fn checked_tree(repo: &Repository, id: Oid) -> Result<git2::Tree<'_>> {
    let (size, kind) = repo.odb()?.read_header(id)?;
    if kind != ObjectType::Tree || size > MAX_BYTES {
        return Err(invalid("Invalid or oversized tree").into());
    }
    Ok(repo.find_tree(id)?)
}

/// Only blobs introduced against the first parent; root commits introduce their whole tree.
/// No patches or unchanged tree snapshots are materialized.
pub(crate) async fn introduced_blobs(path: &Path, commit: &str) -> Result<Vec<TreeObjectEntry>> {
    oid(commit)?;
    let mut command = process::git_command()?;
    command.arg("--git-dir").arg(path).args([
        "diff-tree",
        "--root",
        "--no-commit-id",
        "--diff-merges=first-parent",
        "-r",
        "-z",
        "--raw",
        "--no-renames",
        "--no-abbrev",
        commit,
        "--",
    ]);
    let (status, output) =
        process::buffered(command, b"", 32 * 1024 * 1024, process::RPC_TIMEOUT).await?;
    ensure!(status.success(), "Git could not list introduced blobs");
    let mut fields = output.split(|byte| *byte == 0);
    let mut blobs = Vec::new();
    while let Some(header) = fields.next().filter(|field| !field.is_empty()) {
        let raw = fields.next().context("Git change has no path")?;
        let parts = std::str::from_utf8(header)?
            .split_whitespace()
            .collect::<Vec<_>>();
        ensure!(
            parts.len() == 5 && parts[0].starts_with(':'),
            "Malformed Git change"
        );
        if parts[1] == "000000" || parts[1] == "160000" || parts[2] == parts[3] {
            continue;
        }
        oid(parts[3])?;
        blobs.push(TreeObjectEntry {
            path: String::from_utf8_lossy(raw).into_owned(),
            path_raw: raw.to_vec(),
            mode: parts[1].into(),
            kind: "blob".into(),
            sha: parts[3].into(),
            size: None,
        });
    }
    let path = path.to_owned();
    process::blocking_until(
        std::time::Instant::now() + process::RPC_TIMEOUT,
        move || {
            let repo = Repository::open_bare(path)?;
            let odb = repo.odb()?;
            for blob in &mut blobs {
                let (size, kind) = odb.read_header(oid(&blob.sha)?)?;
                ensure!(kind == ObjectType::Blob, "Invalid introduced blob");
                blob.size = Some(size);
            }
            Ok(blobs)
        },
    )
    .await
}

pub(crate) async fn read_tree(path: &Path, revision: &str, recursive: bool) -> Result<TreeObject> {
    use super::repository::process;
    let deadline = std::time::Instant::now() + process::RPC_TIMEOUT;
    let (owned_path, revision) = (path.to_owned(), revision.to_owned());
    let id = process::blocking_until(deadline, move || {
        let repo = Repository::open_bare(owned_path)?;
        let (id, kind) = super::repository::peel_revision(&repo, &revision)?;
        Ok(if kind == ObjectType::Commit {
            super::repository::find_commit_checked(&repo, id)?.tree_id()
        } else {
            id
        })
    })
    .await?;
    let mut result = TreeObject {
        sha: id.to_string(),
        tree: Vec::new(),
        truncated: false,
    };
    let mut pending = vec![(id.to_string(), Vec::new(), 0usize)];
    let mut path_bytes = 0usize;
    while let Some((id, prefix, depth)) = pending.pop() {
        let (owned_path, tree_id) = (path.to_owned(), id.clone());
        process::blocking_until(deadline, move || {
            let repo = Repository::open_bare(owned_path)?;
            let (size, kind) = repo.odb()?.read_header(oid(&tree_id)?)?;
            anyhow::ensure!(
                kind == ObjectType::Tree && size <= MAX_BYTES,
                invalid("Invalid or oversized tree")
            );
            Ok(())
        })
        .await?;
        // Header sizes alone do not bound packed delta bases. Only the resource-limited
        // child inflates tree payloads, including very large bases of small delta trees.
        let mut command = process::git_command()?;
        command
            .arg("--git-dir")
            .arg(path)
            .args(["ls-tree", "-z", "-l", &id]);
        let (status, bytes) = process::buffered(
            command,
            b"",
            4 * MAX_BYTES,
            deadline.saturating_duration_since(std::time::Instant::now()),
        )
        .await?;
        anyhow::ensure!(status.success(), "Git could not read tree");
        let mut subtrees = Vec::new();
        for record in bytes
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(|| invalid("Invalid tree entry"))?;
            let fields = std::str::from_utf8(&record[..tab])?
                .split_ascii_whitespace()
                .collect::<Vec<_>>();
            anyhow::ensure!(fields.len() == 4, invalid("Invalid tree entry"));
            let mut raw = prefix.clone();
            raw.extend_from_slice(&record[tab + 1..]);
            path_bytes = path_bytes.saturating_add(raw.len());
            if recursive
                && (result.tree.len() == MAX_ENTRIES || path_bytes > MAX_BYTES || depth > MAX_DEPTH)
            {
                result.truncated = true;
                return Ok(result);
            }
            let entry = TreeObjectEntry {
                path: String::from_utf8_lossy(&raw).into_owned(),
                path_raw: raw.clone(),
                mode: fields[0].into(),
                kind: fields[1].into(),
                sha: fields[2].into(),
                size: if fields[3] == "-" {
                    None
                } else {
                    Some(fields[3].parse()?)
                },
            };
            if recursive && entry.kind == "tree" {
                raw.push(b'/');
                subtrees.push((entry.sha.clone(), raw, depth + 1));
            }
            result.tree.push(entry);
        }
        pending.extend(subtrees.into_iter().rev());
    }
    result.tree.sort_by(|a, b| a.path_raw.cmp(&b.path_raw));
    Ok(result)
}

pub(crate) fn ref_target(
    path: &Path,
    reference: &str,
    sha: &str,
) -> Result<(String, String, bool)> {
    if !super::is_canonical_ref(reference) {
        return Err(invalid("Invalid reference name").into());
    }
    let repo = Repository::open_bare(path)?;
    let old = repo
        .find_reference(reference)?
        .target()
        .ok_or_else(|| invalid("Reference must be direct"))?;
    let target = oid(sha)?;
    let (_, kind) = repo.odb()?.read_header(target)?;
    if reference.starts_with("refs/heads/") && kind != ObjectType::Commit {
        return Err(invalid("Branches must point to commits").into());
    }
    let old_kind = repo.odb()?.read_header(old)?.1;
    Ok((
        old.to_string(),
        target.to_string(),
        old_kind == ObjectType::Commit && kind == ObjectType::Commit,
    ))
}

pub(crate) fn object_kind(path: &Path, sha: &str) -> Result<String> {
    let repo = Repository::open_bare(path)?;
    let odb = repo.odb()?;
    Ok(odb.read_header(oid(sha)?)?.1.str().into())
}

pub(crate) fn read_tag(path: &Path, sha: &str) -> Result<TagObject> {
    let repo = Repository::open_bare(path)?;
    let id = oid(sha)?;
    let (size, _) = repo.odb()?.read_header(id)?;
    if size > MAX_BYTES {
        return Err(invalid("Tag exceeds its limit").into());
    }
    let tag = repo.find_tag(id)?;
    let odb = repo.odb()?;
    let raw = odb.read(id)?;
    // The signature covers the exact bytes before it, so the tag is split as bytes.
    let raw = raw.data();
    let signature_start = [
        b"\n-----BEGIN PGP SIGNATURE-----".as_slice(),
        b"\n-----BEGIN SSH SIGNATURE-----",
        b"\n-----BEGIN SIGNED MESSAGE-----",
    ]
    .iter()
    .filter_map(|marker| {
        raw.windows(marker.len())
            .position(|window| window == *marker)
            .map(|offset| offset + 1)
    })
    .min()
    // The signature must be one armor block that ends the tag: text after it is not signed.
    .filter(|offset| {
        let block = raw[*offset..].trim_ascii_end();
        let end: &[u8] = if block.starts_with(b"-----BEGIN PGP") {
            b"-----END PGP SIGNATURE-----"
        } else if block.starts_with(b"-----BEGIN SSH") {
            b"-----END SSH SIGNATURE-----"
        } else {
            b"-----END SIGNED MESSAGE-----"
        };
        block.ends_with(end)
            && block
                .windows(end.len())
                .filter(|window| *window == end)
                .count()
                == 1
    });
    let (signature, signature_payload) = signature_start.map_or((None, None), |offset| {
        (
            Some(String::from_utf8_lossy(&raw[offset..]).into_owned()),
            Some(raw[..offset].to_vec()),
        )
    });
    let tagger = tag.tagger().map(|tagger| {
        let zone = chrono::FixedOffset::east_opt(tagger.when().offset_minutes() * 60);
        let date = chrono::DateTime::from_timestamp(tagger.when().seconds(), 0)
            .zip(zone)
            .map(|(time, zone)| time.with_timezone(&zone));
        Signature {
            name: tagger.name().unwrap_or("").into(),
            email: tagger.email().unwrap_or("").into(),
            date,
        }
    });
    Ok(TagObject {
        sha: id.to_string(),
        tag: String::from_utf8_lossy(tag.name_bytes()).into_owned(),
        message: String::from_utf8_lossy(
            raw.windows(2)
                .position(|bytes| bytes == b"\n\n")
                .map_or(&[][..], |offset| &raw[offset + 2..]),
        )
        .into_owned(),
        object: tag.target_id().to_string(),
        kind: tag
            .target_type()
            .ok_or_else(|| invalid("Invalid tag target"))?
            .str()
            .into(),
        tagger,
        signature,
        signature_payload,
    })
}

/// Every changed directory's builder is held until the end, so their trees share this budget.
const MAX_HELD_TREE_BYTES: usize = 16 * MAX_BYTES;

/// A tree that `change_trees` will keep as a builder, counted against its budget.
fn held_tree<'repo>(
    repo: &'repo Repository,
    id: Oid,
    held: &mut usize,
) -> Result<git2::Tree<'repo>> {
    let tree = checked_tree(repo, id)?;
    *held = held.saturating_add(repo.odb()?.read_header(id)?.0);
    if *held > MAX_HELD_TREE_BYTES {
        return Err(invalid("The changed directories are too large").into());
    }
    Ok(tree)
}

fn write_tree(builder: &mut git2::TreeBuilder<'_>) -> Result<Oid> {
    let mut size = 0usize;
    let mut valid = true;
    builder.filter(|entry| {
        size = size.saturating_add(
            format!("{:o}", entry.filemode()).len() + 1 + entry.name_bytes().len() + 1 + 20,
        );
        valid &= entry.name_bytes().len() <= 255
            && !(entry.name_bytes().eq_ignore_ascii_case(b".gitmodules")
                && entry.filemode() == 0o120000);
        true
    })?;
    if !valid || size > MAX_BYTES {
        return Err(invalid("Invalid or oversized resulting tree").into());
    }
    Ok(builder.write()?)
}

/// Applies `changes` to the tree `base`: each is a slash-separated path and the entry to put
/// there, or `None` to delete it. A directory entry applies before the paths inside it, whatever
/// the request order, so `dir/file` changes the tree given for `dir` instead of being replaced
/// by it; changes to the same path keep their order. Every changed directory stays a builder in
/// memory until the last change, so each new tree is written once, deepest first.
fn change_trees(
    repo: &Repository,
    base: Option<Oid>,
    changes: &[(String, Option<(Oid, i32)>)],
) -> Result<Oid> {
    let mut changes = changes.to_vec();
    changes.sort_by_key(|(path, _)| path.matches('/').count());
    let mut held = 0;
    let root = base.map(|id| held_tree(repo, id, &mut held)).transpose()?;
    // Keyed by directory path; "" is the root.
    let mut builders = BTreeMap::from([(String::new(), repo.treebuilder(root.as_ref())?)]);
    for (path, target) in &changes {
        let (name, parents) = path
            .rsplit_once('/')
            .map_or((path.as_str(), ""), |(parents, name)| (name, parents));
        let mut directory = String::new();
        for part in parents.split('/').filter(|_| !parents.is_empty()) {
            let child = if directory.is_empty() {
                part.to_owned()
            } else {
                format!("{directory}/{part}")
            };
            if !builders.contains_key(&child) {
                let entry = builders[&directory]
                    .get(part)?
                    .map(|entry| (entry.id(), entry.kind()));
                if entry.is_some_and(|(_, kind)| kind != Some(ObjectType::Tree)) {
                    return Err(invalid("Tree path crosses a file").into());
                }
                let tree = entry
                    .map(|(id, _)| held_tree(repo, id, &mut held))
                    .transpose()?;
                builders.insert(child.clone(), repo.treebuilder(tree.as_ref())?);
            }
            directory = child;
        }
        // A directory changed earlier in this request is replaced, with everything below it.
        let replaced = builders.contains_key(path.as_str());
        if replaced {
            let below = format!("{path}/");
            builders.retain(|key, _| key != path && !key.starts_with(&below));
        }
        let builder = builders
            .get_mut(&directory)
            .ok_or_else(|| invalid("Invalid tree path"))?;
        match target {
            Some((id, mode)) => {
                builder.insert(name, *id, *mode)?;
            }
            None => {
                if builder.get(name)?.is_some() {
                    builder.remove(name)?;
                } else if !replaced {
                    return Err(invalid("Cannot delete a missing tree entry").into());
                }
            }
        }
    }
    let mut directories: Vec<String> = builders
        .keys()
        .filter(|key| !key.is_empty())
        .cloned()
        .collect();
    directories.sort_by_key(|key| std::cmp::Reverse(key.matches('/').count()));
    for directory in directories {
        let mut builder = builders
            .remove(&directory)
            .ok_or_else(|| invalid("Invalid tree path"))?;
        let id = if builder.is_empty() {
            None
        } else {
            Some(write_tree(&mut builder)?)
        };
        let (parent, name) = directory
            .rsplit_once('/')
            .unwrap_or(("", directory.as_str()));
        let parent = builders
            .get_mut(parent)
            .ok_or_else(|| invalid("Invalid tree path"))?;
        if let Some(id) = id {
            parent.insert(name, id, 0o040000)?;
        } else if parent.get(name)?.is_some() {
            parent.remove(name)?;
        }
    }
    let mut root = builders
        .remove("")
        .ok_or_else(|| invalid("Invalid tree path"))?;
    write_tree(&mut root)
}

pub(crate) fn write(path: &Path, input: ObjectWrite, bytes: &[u8]) -> Result<String> {
    let repo = Repository::open_bare(path)?;
    let id = match input {
        ObjectWrite::Blob => {
            if bytes.len() > MAX_BLOB_WRITE_BYTES {
                return Err(invalid("Blob exceeds its limit").into());
            }
            repo.blob(bytes)?
        }
        ObjectWrite::Tree { base_tree, tree } => {
            if tree.len() > MAX_ENTRIES {
                return Err(invalid("Too many tree entries").into());
            }
            let base = base_tree.as_deref().map(oid).transpose()?;
            let mut size = 0usize;
            let mut changes = Vec::with_capacity(tree.len());
            for entry in tree {
                size = size
                    .saturating_add(entry.path.len())
                    .saturating_add(entry.content.as_ref().map_or(0, String::len));
                let path: Vec<_> = entry.path.split('/').collect();
                if size > MAX_BYTES
                    || path.len() > MAX_DEPTH
                    || path.iter().any(|part| {
                        part.is_empty()
                            || part.len() > 255
                            || matches!(*part, "." | ".." | ".git")
                            || part.contains('\0')
                    })
                {
                    return Err(invalid("Invalid or oversized tree path").into());
                }
                let mode = match (entry.mode.as_str(), entry.kind.as_str()) {
                    ("100644", "blob") => 0o100644,
                    ("100755", "blob") => 0o100755,
                    ("120000", "blob") => 0o120000,
                    ("040000", "tree") => 0o040000,
                    ("160000", "commit") => 0o160000,
                    _ => return Err(invalid("Tree mode does not match its type").into()),
                };
                if mode == 0o120000
                    && path
                        .last()
                        .is_some_and(|name| name.eq_ignore_ascii_case(".gitmodules"))
                {
                    return Err(invalid(".gitmodules must not be a symlink").into());
                }
                let target = match (entry.sha, entry.content) {
                    (Some(_), Some(_)) => {
                        return Err(invalid("Specify sha or content, not both").into())
                    }
                    (Some(sha), None) => {
                        let id = oid(&sha)?;
                        if mode != 0o160000 {
                            let (_, actual) = repo.odb()?.read_header(id)?;
                            if actual != kind(&entry.kind)? {
                                return Err(invalid("Tree object type does not match").into());
                            }
                        }
                        Some((id, mode))
                    }
                    (None, Some(content)) if entry.kind == "blob" => {
                        Some((repo.blob(content.as_bytes())?, mode))
                    }
                    (None, Some(_)) => return Err(invalid("Only blobs accept content").into()),
                    (None, None) => None,
                };
                changes.push((entry.path, target));
            }
            change_trees(&repo, base, &changes)?
        }
        ObjectWrite::Commit {
            message,
            tree,
            parents,
            author,
            committer,
            signature,
        } => {
            if message.len() > MAX_BYTES
                || message.contains('\0')
                || parents.len() > 100
                || signature
                    .as_ref()
                    .is_some_and(|value| value.len() > MAX_BYTES || value.contains('\0'))
            {
                return Err(invalid("Invalid commit metadata").into());
            }
            let tree = checked_tree(&repo, oid(&tree)?)?;
            let parents: Vec<_> = parents
                .iter()
                .map(|sha| super::repository::find_commit_checked(&repo, oid(sha)?))
                .collect::<Result<_>>()?;
            let refs: Vec<_> = parents.iter().collect();
            let buffer = repo.commit_create_buffer(
                &author.git()?,
                &committer.git()?,
                &message,
                &tree,
                &refs,
            )?;
            let signature_bytes = signature
                .as_ref()
                .map_or(0, |value| value.len() + value.lines().count() + 8);
            if buffer.len() + signature_bytes > super::MAX_COMMIT_OBJECT_BYTES {
                return Err(invalid("Commit exceeds its limit").into());
            }
            match signature {
                Some(signature) => {
                    repo.commit_signed(std::str::from_utf8(&buffer)?, &signature, None)?
                }
                None => repo.odb()?.write(ObjectType::Commit, &buffer)?,
            }
        }
        ObjectWrite::Tag {
            tag,
            message,
            object,
            kind: target_kind,
            tagger,
        } => {
            if super::tag_ref(&tag).is_none()
                || message.len() > super::MAX_TAG_MESSAGE_BYTES
                || message.contains('\0')
            {
                return Err(invalid("Invalid tag metadata").into());
            }
            let id = oid(&object)?;
            // The header gives the target's type, so a tag of a large blob never loads the blob.
            let (_, actual) = repo.odb()?.read_header(id)?;
            if actual != kind(&target_kind)? {
                return Err(invalid("Tag target type does not match").into());
            }
            let text = annotation(id, actual, &tag, &tagger.git()?, &message);
            repo.odb()?.write(ObjectType::Tag, text.as_bytes())?
        }
    };
    Ok(id.to_string())
}

/// The text of an annotated tag object, as `tag_annotation_create` writes it: the message is
/// kept exactly as given, so it reads back unchanged, as GitHub returns it.
pub(crate) fn annotation(
    target: Oid,
    kind: ObjectType,
    tag: &str,
    tagger: &git2::Signature<'_>,
    message: &str,
) -> String {
    let when = tagger.when();
    let offset = when.offset_minutes();
    format!(
        "object {target}\ntype {}\ntag {tag}\ntagger {} <{}> {} {}{:02}{:02}\n\n{message}",
        kind.str(),
        tagger.name().unwrap_or_default(),
        tagger.email().unwrap_or_default(),
        when.seconds(),
        if offset < 0 { '-' } else { '+' },
        offset.abs() / 60,
        offset.abs() % 60,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(repo: &Repository, id: Oid) -> Vec<String> {
        let mut found = Vec::new();
        repo.find_tree(id)
            .unwrap()
            .walk(git2::TreeWalkMode::PreOrder, |root, entry| {
                found.push(format!("{root}{}", entry.name().unwrap()));
                git2::TreeWalkResult::Ok
            })
            .unwrap();
        found
    }

    #[test]
    fn a_tag_signature_must_end_the_tag() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(root.path()).unwrap();
        let blob = repo.blob(b"text").unwrap();
        let tag = |suffix: &str| {
            let raw = format!("object {blob}\ntype blob\ntag v1\ntagger A <a@example.test> 0 +0000\n\nRelease\n-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----\n{suffix}");
            let id = repo
                .odb()
                .unwrap()
                .write(git2::ObjectType::Tag, raw.as_bytes())
                .unwrap();
            read_tag(root.path(), &id.to_string()).unwrap()
        };
        assert!(tag("").signature.is_some());
        let appended = tag("Unsigned instructions\n");
        assert!(appended.signature.is_none() && appended.signature_payload.is_none());
    }

    #[test]
    fn tree_changes_apply_directories_first_and_write_each_tree_once() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(root.path()).unwrap();
        let blob = repo.blob(b"text").unwrap();
        let file = Some((blob, 0o100644));
        let change = |path: &str, target| (path.to_owned(), target);
        let base = change_trees(
            &repo,
            None,
            &[
                change("a/b/one", file),
                change("a/b/two", file),
                change("c", file),
            ],
        )
        .unwrap();
        assert_eq!(paths(&repo, base), ["a", "a/b", "a/b/one", "a/b/two", "c"]);
        // A directory deleted in the same request goes first, so the paths put below it stay.
        let changed = change_trees(
            &repo,
            Some(base),
            &[
                change("a/b/three", file),
                change("a", None),
                change("a/new", file),
                change("c", None),
            ],
        )
        .unwrap();
        assert_eq!(paths(&repo, changed), ["a", "a/b", "a/b/three", "a/new"]);
        assert!(change_trees(&repo, Some(base), &[change("c/inside", file)]).is_err());
        assert!(change_trees(&repo, Some(base), &[change("missing", None)]).is_err());
    }
}

//! Flat, bounded gist trees and immutable revision reads, shared by API saves and receive hooks.
use crate::{
    error::{ErrorKind, RepoError},
    gist_types::{
        valid_gist_filename, GistEdit, GistRevision, GistSnapshot, PreparedGist, MAX_GIST_BYTES,
        MAX_GIST_FILENAME_BYTES, MAX_GIST_FILES,
    },
    objects::TreeObjectEntry,
};
use anyhow::{ensure, Result};
use git2::{Commit, ObjectType, Oid, Repository};
use std::{
    collections::{BTreeMap, BinaryHeap, HashMap, HashSet},
    path::Path,
    time::Instant,
};

fn invalid() -> RepoError {
    RepoError::new(
        ErrorKind::Invalid,
        "A gist must contain 1–100 flat regular files within the size limit",
    )
}
fn missing() -> RepoError {
    RepoError::new(ErrorKind::NotFound, "Gist revision not found")
}
fn head(repo: &Repository) -> Result<Option<Commit<'_>>> {
    match repo.find_reference("refs/heads/main") {
        Ok(reference) => Ok(Some(reference.peel_to_commit()?)),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
/// A valid root tree has at most `MAX_GIST_FILES` entries, each a mode, a space, a name, a NUL
/// and a 20-byte object ID.
const MAX_GIST_TREE_BYTES: usize = MAX_GIST_FILES * (6 + 1 + MAX_GIST_FILENAME_BYTES + 1 + 20);

fn entries(repo: &Repository, commit: &Commit<'_>) -> Result<Vec<TreeObjectEntry>> {
    let odb = repo.odb()?;
    // Check the header before loading the tree, so an oversized one is never inflated.
    let (tree_bytes, _) = odb.read_header(commit.tree_id())?;
    ensure!(tree_bytes <= MAX_GIST_TREE_BYTES, invalid());
    let tree = commit.tree()?;
    ensure!(!tree.is_empty() && tree.len() <= MAX_GIST_FILES, invalid());
    let mut total = 0usize;
    let mut files = Vec::new();
    let mut previous_name: Option<String> = None;
    for entry in &tree {
        let name = entry.name().ok_or_else(invalid)?;
        ensure!(
            previous_name
                .as_deref()
                .is_none_or(|previous| previous < name),
            invalid()
        );
        previous_name = Some(name.to_owned());
        ensure!(
            valid_gist_filename(name)
                && entry.kind() == Some(ObjectType::Blob)
                && matches!(entry.filemode(), 0o100644 | 0o100755),
            invalid()
        );
        let (size, kind) = odb.read_header(entry.id())?;
        total = total.saturating_add(size);
        ensure!(
            kind == ObjectType::Blob && total <= MAX_GIST_BYTES,
            RepoError::response_too_large()
        );
        files.push(TreeObjectEntry {
            path: name.into(),
            path_raw: name.as_bytes().to_vec(),
            mode: format!("{:06o}", entry.filemode()),
            kind: "blob".into(),
            sha: entry.id().to_string(),
            size: Some(size),
        });
    }
    Ok(files)
}
pub fn revision_time(
    timestamp: i64,
) -> std::result::Result<chrono::DateTime<chrono::Utc>, RepoError> {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .filter(|date| (1..=9999).contains(&chrono::Datelike::year(date)))
        .ok_or_else(|| RepoError::new(ErrorKind::Invalid, "Invalid gist commit timestamp"))
}
struct HistoryNode {
    parents: Vec<Oid>,
    timestamp: i64,
}
struct HistoryGraph {
    nodes: HashMap<Oid, HistoryNode>,
    deadline: Instant,
}
// Sorted and hidden libgit2 walks prepare the entire graph before yielding a commit.
// Read headers before payloads and bound both graph discovery and subsequent ordering.
fn bounded_graph(repo: &Repository, tip: Oid) -> Result<HistoryGraph> {
    let deadline = Instant::now() + super::COMMIT_WALK_TIMEOUT;
    let mut pending = vec![tip];
    let mut seen = HashSet::from([tip]);
    let odb = repo.odb()?;
    let mut bytes = 0usize;
    let mut nodes = HashMap::new();
    while let Some(oid) = pending.pop() {
        ensure!(Instant::now() < deadline, RepoError::response_too_large());
        let (size, kind) = odb.read_header(oid)?;
        bytes = bytes.saturating_add(size);
        ensure!(
            kind == ObjectType::Commit && bytes <= super::repository::MAX_COMMIT_WALK_BYTES,
            RepoError::response_too_large()
        );
        let commit = repo.find_commit(oid)?;
        let mut parents = Vec::new();
        for parent in commit.parent_ids() {
            ensure!(Instant::now() < deadline, RepoError::response_too_large());
            if seen.insert(parent) {
                ensure!(
                    seen.len() <= super::repository::MAX_COMMIT_WALK_OBJECTS,
                    RepoError::response_too_large()
                );
                pending.push(parent);
            }
            parents.push(parent);
        }
        nodes.insert(
            oid,
            HistoryNode {
                parents,
                timestamp: commit.time().seconds(),
            },
        );
    }
    ensure!(Instant::now() < deadline, RepoError::response_too_large());
    Ok(HistoryGraph { nodes, deadline })
}
fn selected_commit<'a>(repo: &'a Repository, revision: Option<&str>) -> Result<Commit<'a>> {
    let head = head(repo)?.ok_or_else(missing)?;
    let commit = if let Some(revision) = revision {
        ensure!(
            (7..=40).contains(&revision.len()) && revision.bytes().all(|b| b.is_ascii_hexdigit()),
            RepoError::new(ErrorKind::Invalid, "Invalid gist revision")
        );
        let revision = revision.to_ascii_lowercase();
        if revision == head.id().to_string() {
            return Ok(head);
        }
        let graph = bounded_graph(repo, head.id())?;
        let mut matches = graph
            .nodes
            .keys()
            .filter(|oid| oid.to_string().starts_with(&revision));
        let oid = *matches.next().ok_or_else(missing)?;
        ensure!(matches.next().is_none(), missing());
        ensure!(
            Instant::now() < graph.deadline,
            RepoError::response_too_large()
        );
        // Never parse an arbitrary unreachable object supplied by the caller.
        repo.find_commit(oid)?
    } else {
        head
    };
    Ok(commit)
}
pub fn snapshot(path: &Path, revision: Option<&str>) -> Result<GistSnapshot> {
    let repo = Repository::open_bare(path)?;
    let commit = selected_commit(&repo, revision)?;
    Ok(GistSnapshot {
        sha: commit.id().to_string(),
        files: entries(&repo, &commit)?,
    })
}
pub fn prepare(path: &Path, input: &GistEdit, name: &str, email: &str) -> Result<PreparedGist> {
    input.validate()?;
    let repo = Repository::open_bare(path)?;
    let parent = head(&repo)?;
    let base = parent.as_ref().map(|commit| commit.id().to_string());
    ensure!(
        input
            .expected
            .as_ref()
            .is_none_or(|expected| Some(expected) == base.as_ref()),
        RepoError::new(ErrorKind::Conflict, "Gist changed; reload before saving")
    );
    let original = if let Some(parent) = &parent {
        entries(&repo, parent)?
            .into_iter()
            .map(|entry| {
                (
                    entry.path,
                    (
                        Oid::from_str(&entry.sha).expect("stored Git object ID"),
                        entry.mode,
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>()
    } else {
        BTreeMap::new()
    };
    let mut files = original
        .iter()
        .map(|(name, (oid, mode))| (name.clone(), (*oid, mode.clone(), None)))
        .collect::<BTreeMap<_, _>>();
    for (name, change) in &input.files {
        if change
            .as_ref()
            .is_some_and(|change| change.filename.is_none() && change.content.is_none())
        {
            ensure!(original.contains_key(name), invalid());
        }
        files.remove(name);
    }
    let mut destinations = HashSet::new();
    for (name, change) in &input.files {
        let Some(change) = change else { continue };
        if change.filename.is_none() && change.content.is_none() {
            continue;
        }
        let target = change.filename.as_ref().unwrap_or(name);
        ensure!(
            !files.contains_key(target) && destinations.insert(target.clone()),
            RepoError::new(ErrorKind::Invalid, "Gist filenames must be unique")
        );
        let (oid, mode, content) = if let Some(content) = &change.content {
            (
                Oid::zero(),
                original
                    .get(name)
                    .map(|(_, mode)| mode.clone())
                    .unwrap_or_else(|| "100644".to_owned()),
                Some(content.as_str()),
            )
        } else {
            let (oid, mode) = original.get(name).cloned().ok_or_else(invalid)?;
            (oid, mode, None)
        };
        files.insert(target.clone(), (oid, mode, content));
    }
    ensure!(
        !files.is_empty() && files.len() <= MAX_GIST_FILES,
        invalid()
    );
    let mut total = 0usize;
    for (oid, _, content) in files.values() {
        let size = match content {
            Some(content) => content.len(),
            None => repo.odb()?.read_header(*oid)?.0,
        };
        total = total.saturating_add(size);
        ensure!(total <= MAX_GIST_BYTES, RepoError::response_too_large());
    }
    let mut builder = repo.treebuilder(None)?;
    for (name, (oid, mode, content)) in files {
        let oid = match content {
            Some(content) => repo.blob(content.as_bytes())?,
            None => oid,
        };
        builder.insert(
            name,
            oid,
            if mode == "100755" { 0o100755 } else { 0o100644 },
        )?;
    }
    let tree = repo.find_tree(builder.write()?)?;
    let signature = git2::Signature::now(name, email)?;
    let parents = parent.iter().collect::<Vec<_>>();
    let sha = repo.commit(
        None,
        &signature,
        &signature,
        if parent.is_some() {
            "Update gist"
        } else {
            "Create gist"
        },
        &tree,
        &parents,
    )?;
    bounded_graph(&repo, sha)?;
    Ok(PreparedGist {
        base,
        sha: sha.to_string(),
    })
}
fn history_metadata(
    path: &Path,
    revision: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<Vec<(GistRevision, Option<String>)>> {
    let repo = Repository::open_bare(path)?;
    let head = selected_commit(&repo, revision)?;
    let graph = bounded_graph(&repo, head.id())?;
    let mut children: HashMap<Oid, usize> = graph.nodes.keys().map(|oid| (*oid, 0)).collect();
    for node in graph.nodes.values() {
        for parent in &node.parents {
            *children.get_mut(parent).ok_or_else(missing)? += 1;
        }
    }
    // A future-dated parent must stay after its children, including across merges.
    let mut ready = BinaryHeap::new();
    for (oid, count) in &children {
        if *count == 0 {
            ready.push((graph.nodes[oid].timestamp, *oid));
        }
    }
    let mut result = Vec::new();
    let mut position = 0usize;
    while let Some((_, oid)) = ready.pop() {
        ensure!(
            Instant::now() < graph.deadline,
            RepoError::response_too_large()
        );
        if result.len() >= limit.min(101) {
            break;
        }
        for parent in &graph.nodes[&oid].parents {
            let count = children.get_mut(parent).ok_or_else(missing)?;
            *count = count.checked_sub(1).ok_or_else(missing)?;
            if *count == 0 {
                ready.push((graph.nodes[parent].timestamp, *parent));
            }
        }
        position += 1;
        if position <= offset {
            continue;
        }
        let commit = repo.find_commit(oid)?;
        entries(&repo, &commit)?;
        let previous = if commit.parent_count() > 0 {
            Some(commit.parent_id(0)?.to_string())
        } else {
            None
        };
        result.push((
            GistRevision {
                sha: oid.to_string(),
                author_email: commit
                    .author()
                    .email()
                    .filter(|email| email.len() <= super::repository::MAX_COMMIT_EMAIL_BYTES)
                    .unwrap_or_default()
                    .into(),
                timestamp: commit.time().seconds(),
                additions: 0,
                deletions: 0,
            },
            previous,
        ));
    }
    Ok(result)
}
/// `lock` stays held until the walk and the change counts finish, and until the blocking walk
/// ends even when the request is cancelled.
pub async fn history(
    path: &Path,
    revision: Option<&str>,
    offset: usize,
    limit: usize,
    lock: tokio::sync::OwnedMutexGuard<()>,
) -> Result<Vec<GistRevision>> {
    use super::repository::process;
    let lock = std::sync::Arc::new(lock);
    let walk_lock = lock.clone();
    let owned_path = path.to_owned();
    let revision = revision.map(str::to_owned);
    let entries = process::blocking_until(Instant::now() + super::COMMIT_WALK_TIMEOUT, move || {
        let _lock = walk_lock;
        history_metadata(&owned_path, revision.as_deref(), offset, limit)
    })
    .await?;
    let mut result = Vec::new();
    for (mut revision, previous) in entries {
        let mut command = process::git_command()?;
        command.arg("--git-dir").arg(path);
        match previous {
            Some(previous) => {
                command.args([
                    "diff",
                    "--numstat",
                    "-z",
                    "--find-renames",
                    &previous,
                    &revision.sha,
                    "--",
                ]);
            }
            None => {
                command.args([
                    "diff-tree",
                    "--root",
                    "-r",
                    "--no-commit-id",
                    "--numstat",
                    "-z",
                    &revision.sha,
                    "--",
                ]);
            }
        }
        let (status, bytes) =
            process::buffered(command, b"", 128 * 1024, super::COMMIT_WALK_TIMEOUT).await?;
        ensure!(
            status.success(),
            "Git could not read gist change statistics"
        );
        for record in bytes
            .split(|byte| *byte == 0)
            .filter(|record| record.contains(&b'\t'))
        {
            let mut fields = record.splitn(3, |byte| *byte == b'\t');
            let count = |field: &[u8]| -> Result<i32> {
                if field == b"-" {
                    Ok(0)
                } else {
                    Ok(std::str::from_utf8(field)?.parse()?)
                }
            };
            revision.additions += count(fields.next().ok_or_else(invalid)?)?;
            revision.deletions += count(fields.next().ok_or_else(invalid)?)?;
        }
        result.push(revision);
    }
    drop(lock);
    Ok(result)
}

pub fn validate_push(
    path: &Path,
    quarantine: Option<&Path>,
    old: &str,
    revision: &str,
) -> Result<()> {
    let repo = super::repository::open_quarantined(path, quarantine)?;
    let graph = bounded_graph(&repo, Oid::from_str(revision)?)?;
    let previous = if old == "0000000000000000000000000000000000000000" {
        None
    } else {
        Some(bounded_graph(&repo, Oid::from_str(old)?)?)
    };
    for oid in graph.nodes.keys() {
        ensure!(
            Instant::now() < graph.deadline,
            RepoError::response_too_large()
        );
        if previous
            .as_ref()
            .is_some_and(|old| old.nodes.contains_key(oid))
        {
            continue;
        }
        let commit = repo.find_commit(*oid)?;
        revision_time(commit.time().seconds())?;
        entries(&repo, &commit)?;
    }
    // Even a rewind to an already reachable commit must preserve the current tree invariant.
    let target = repo.find_commit(Oid::from_str(revision)?)?;
    revision_time(target.time().seconds())?;
    entries(&repo, &target)?;
    Ok(())
}

//! Git reads behind CI admission: which commit a run will use, and the workflow
//! files inside it.
use anyhow::{ensure, Context, Result};
use git2::{ObjectType, Oid, Repository};
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::repository::{find_commit_checked, peel_to_commit_checked, tree_entry};

pub const MAX_CI_WORKFLOW_BYTES: usize = 256 * 1024;
pub const MAX_WORKFLOW_DIRECTORY_ENTRIES: usize = 64;
pub const MAX_WORKFLOW_FILE_NAME_BYTES: usize = 255;
// A serialized tree entry is mode + space + name + NUL + a 20-byte SHA-1.
const MAX_SERIALIZED_TREE_ENTRY_FIXED_BYTES: usize = 6 + 1 + 1 + 20;
pub const MAX_CI_WORKFLOW_TREE_BYTES: usize = MAX_WORKFLOW_DIRECTORY_ENTRIES
    * (MAX_WORKFLOW_FILE_NAME_BYTES + MAX_SERIALIZED_TREE_ENTRY_FIXED_BYTES);

/// The commit a CI run was admitted for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiCommit {
    /// The ref's full name: `refs/heads/...` or `refs/tags/...`.
    pub full_ref: String,
    pub sha: String,
}

/// A workflow file as it exists at one commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiWorkflowFile {
    pub blob_id: String,
    pub content: String,
}

/// A bounded workflow file returned by discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiWorkflowSource {
    pub path: String,
    pub content: String,
}

/// A ref or commit the request names that this repository does not have. It is reported as
/// not found, not as a storage failure.
fn not_found(message: &str) -> anyhow::Error {
    git2::Error::new(
        git2::ErrorCode::InvalidSpec,
        git2::ErrorClass::Invalid,
        message,
    )
    .into()
}

/// Picks the commit for a run. `reference` is a branch or tag name, or a full ref
/// name. Without `sha` the ref's current tip is used. With `must_be_on_ref` the
/// given `sha` has to be reachable from the ref's tip.
pub(crate) fn resolve_ci_commit(
    path: &Path,
    reference: &str,
    sha: Option<&str>,
    must_be_on_ref: bool,
) -> Result<CiCommit> {
    let git = Repository::open_bare(path).context("CI bare repository unavailable")?;
    let full_ref = if reference.starts_with("refs/") {
        reference.to_owned()
    } else if git
        .find_reference(&format!("refs/heads/{reference}"))
        .is_ok()
    {
        format!("refs/heads/{reference}")
    } else {
        format!("refs/tags/{reference}")
    };
    ensure!(
        full_ref.starts_with("refs/heads/") || full_ref.starts_with("refs/tags/"),
        "invalid CI ref kind"
    );
    let tip = peel_to_commit_checked(&git, &full_ref).ok().map(|c| c.id());
    let oid = match sha {
        Some(sha) => {
            if sha.len() != 40 || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err(not_found("invalid CI SHA"));
            }
            let oid = Oid::from_str(sha)?;
            find_commit_checked(&git, oid).context("CI commit unavailable")?;
            if must_be_on_ref {
                let tip = tip.ok_or_else(|| not_found("CI ref unavailable"))?;
                if !bounded_ancestor(&git, tip, oid)? {
                    return Err(not_found("CI SHA is unrelated to requested ref"));
                }
            }
            oid
        }
        None => tip.ok_or_else(|| not_found("CI ref unavailable"))?,
    };
    Ok(CiCommit {
        full_ref,
        sha: oid.to_string(),
    })
}

/// Reads one workflow file from a commit. It has to be a regular tracked text
/// file of reasonable size.
pub(crate) fn read_ci_workflow(path: &Path, sha: &str, workflow: &str) -> Result<CiWorkflowFile> {
    let git = Repository::open_bare(path).context("CI bare repository unavailable")?;
    let commit = find_commit_checked(&git, Oid::from_str(sha)?)?;
    let entry =
        tree_entry(&git, &commit, workflow).context("CI workflow unavailable at admitted SHA")?;
    ensure!(
        matches!(entry.filemode(), 0o100644 | 0o100755) && entry.kind() == Some(ObjectType::Blob),
        "CI workflow must be a regular tracked file"
    );
    let blob = read_bounded_workflow_blob(&git, entry.id())?;
    let content = std::str::from_utf8(blob.content())
        .context("CI workflow must be UTF-8")?
        .to_owned();
    Ok(CiWorkflowFile {
        blob_id: entry.id().to_string(),
        content,
    })
}

/// Lists only direct, regular workflow files at a revision. Directory and object
/// bounds are checked before any workflow blob is materialized.
pub(crate) fn list_ci_workflow_files(path: &Path, revision: &str) -> Result<Vec<CiWorkflowSource>> {
    let git = Repository::open_bare(path)?;
    let commit = peel_to_commit_checked(&git, revision)?;
    workflow_files_in_commit(&git, &commit)
}

fn read_bounded_workflow_blob<'repo>(
    git: &'repo Repository,
    oid: Oid,
) -> Result<git2::Blob<'repo>> {
    let (size, kind) = git.odb()?.read_header(oid)?;
    ensure!(kind == ObjectType::Blob, "CI workflow must be a blob");
    ensure!(
        size <= MAX_CI_WORKFLOW_BYTES,
        "CI workflow exceeds size limit"
    );
    Ok(git.find_blob(oid)?)
}

fn workflow_files_in_commit(
    git: &Repository,
    commit: &git2::Commit<'_>,
) -> Result<Vec<CiWorkflowSource>> {
    let entry = tree_entry(git, commit, ".github/workflows")?;
    let (tree_size, tree_kind) = git.odb()?.read_header(entry.id())?;
    ensure!(
        tree_kind == ObjectType::Tree,
        "CI workflow directory must be a tree"
    );
    ensure!(
        tree_size <= MAX_CI_WORKFLOW_TREE_BYTES,
        "CI workflow directory exceeds serialized size limit"
    );
    let workflows = git.find_tree(entry.id())?;
    ensure!(
        workflows.len() <= MAX_WORKFLOW_DIRECTORY_ENTRIES,
        "CI workflow directory exceeds entry limit"
    );

    let mut files = Vec::new();
    for entry in &workflows {
        let name_bytes = entry.name_bytes();
        if name_bytes.len() > MAX_WORKFLOW_FILE_NAME_BYTES {
            tracing::warn!(commit = %commit.id(), "workflow with oversized file name skipped");
            continue;
        }
        let Ok(name) = std::str::from_utf8(name_bytes) else {
            tracing::warn!(commit = %commit.id(), "workflow with non-UTF-8 file name skipped");
            continue;
        };
        if !matches!(entry.filemode(), 0o100644 | 0o100755) || !is_ci_workflow_filename(name) {
            continue;
        }
        let path = format!(".github/workflows/{name}");
        let blob = match read_bounded_workflow_blob(git, entry.id()) {
            Ok(blob) => blob,
            Err(error) => {
                tracing::warn!(workflow = %path, commit = %commit.id(), %error, "unreadable workflow skipped");
                continue;
            }
        };
        if blob.is_binary() {
            tracing::warn!(workflow = %path, commit = %commit.id(), "binary workflow skipped");
            continue;
        }
        let content = match std::str::from_utf8(blob.content()) {
            Ok(content) => content.to_owned(),
            Err(error) => {
                tracing::warn!(workflow = %path, commit = %commit.id(), %error, "non-UTF-8 workflow skipped");
                continue;
            }
        };
        files.push(CiWorkflowSource { path, content });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

pub(crate) fn is_ci_workflow_filename(name: &str) -> bool {
    name.len() + ".github/workflows/".len() <= 255
        && (name.ends_with(".yml") || name.ends_with(".yaml"))
        && !name.contains("..")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn bounded_ancestor(git: &Repository, tip: Oid, ancestor: Oid) -> Result<bool> {
    let mut pending = std::collections::VecDeque::from([tip]);
    let mut visited = std::collections::HashSet::new();
    while let Some(oid) = pending.pop_front() {
        if oid == ancestor {
            return Ok(true);
        }
        if !visited.insert(oid) {
            continue;
        }
        ensure!(
            visited.len() <= 16384,
            "CI ref ancestry exceeds admission traversal bound"
        );
        let commit = find_commit_checked(git, oid)?;
        ensure!(
            commit.parent_count() <= 64,
            "CI commit has unsupported parent fanout"
        );
        pending.extend(commit.parent_ids());
    }
    Ok(false)
}

/// True when a lookup found no such path in a tree. Other Git errors, such as a missing
/// object, stay errors.
fn is_missing_path(error: &anyhow::Error) -> bool {
    error.downcast_ref::<git2::Error>().is_some_and(|error| {
        error.code() == git2::ErrorCode::NotFound && error.class() == git2::ErrorClass::Tree
    })
}

pub(crate) fn load_push_workflows(
    path: &Path,
    sha: &str,
) -> Result<(String, Vec<(String, String)>)> {
    let git = Repository::open_bare(path)?;
    let commit = peel_to_commit_checked(&git, sha)?;
    let commit_sha = commit.id().to_string();
    let files = match workflow_files_in_commit(&git, &commit) {
        Ok(files) => files,
        // A commit without a workflow directory has no workflows to run.
        Err(error) if is_missing_path(&error) => Vec::new(),
        Err(error) => return Err(error),
    };
    Ok((
        commit_sha,
        files
            .into_iter()
            .map(|file| (file.path, file.content))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_commits_with_large_messages_can_load_push_workflows() {
        let root = tempfile::tempdir().unwrap();
        let git = Repository::init_bare(root.path()).unwrap();
        let tree_id = git.treebuilder(None).unwrap().write().unwrap();
        let tree = git.find_tree(tree_id).unwrap();
        let author = git2::Signature::now("Writer", "writer@example.test").unwrap();
        let sha = git
            .commit(
                Some("refs/heads/main"),
                &author,
                &author,
                &"m".repeat(70 * 1024),
                &tree,
                &[],
            )
            .unwrap();
        let (loaded, workflows) = load_push_workflows(root.path(), &sha.to_string()).unwrap();
        assert_eq!(loaded, sha.to_string());
        assert!(workflows.is_empty());
    }
}

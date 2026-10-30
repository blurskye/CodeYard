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
    let tip = match git.find_reference(&full_ref) {
        Ok(_) => Some(peel_to_commit_checked(&git, &full_ref).map_err(|error| anyhow::anyhow!("CI reference target is unreadable: {error:#}"))?.id()),
        Err(error) if error.code() == git2::ErrorCode::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let oid = match sha {
        Some(sha) => {
            if !super::is_full_commit_id(sha) {
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
        // An oversized workflow is unsupported and left out. A missing or unreadable object fails the
        // listing, so an incomplete result is never cached as the commit's workflows.
        let (size, _) = git.odb()?.read_header(entry.id())?;
        if size > MAX_CI_WORKFLOW_BYTES {
            tracing::warn!(workflow = %path, commit = %commit.id(), "oversized workflow skipped");
            continue;
        }
        let blob = read_bounded_workflow_blob(git, entry.id())?;
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

/// A workflow file directly in `.github/workflows/`. Spaces and non-ASCII letters are valid, as
/// on GitHub. A separator, a comma or a control character is not, and neither is a colon,
/// which job identities use to split the path from the job.
pub(crate) fn is_ci_workflow_filename(name: &str) -> bool {
    name.len() + ".github/workflows/".len() <= 255
        && (name.ends_with(".yml") || name.ends_with(".yaml"))
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ',' | ':'))
}

fn bounded_ancestor(git: &Repository, tip: Oid, ancestor: Oid) -> Result<bool> {
    let mut pending = std::collections::VecDeque::from([tip]);
    let mut visited = std::collections::HashSet::new();
    let deadline = std::time::Instant::now() + super::repository::COMMIT_WALK_TIMEOUT;
    let mut bytes = 0usize;
    let odb = git.odb()?;
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
        ensure!(std::time::Instant::now() < deadline, "CI ancestry exceeded its time budget");
        bytes = bytes.checked_add(odb.read_header(oid)?.0).filter(|bytes| *bytes <= super::repository::MAX_COMMIT_WALK_BYTES).ok_or(super::ObjectTooLarge)?;
        let commit = find_commit_checked(git, oid)?;
        ensure!(
            commit.parent_count() <= 64,
            "CI commit has unsupported parent fanout"
        );
        pending.extend(commit.parent_ids());
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_commits_with_large_messages_can_list_workflows() {
        let root = tempfile::tempdir().unwrap();
        let git = Repository::init_bare(root.path()).unwrap();
        let directory = |name: &str, id: Oid, mode: i32| {
            let mut tree = git.treebuilder(None).unwrap();
            tree.insert(name, id, mode).unwrap();
            tree.write().unwrap()
        };
        let workflow = git.blob(b"on: push\n").unwrap();
        let workflows = directory("ci.yml", workflow, 0o100644);
        let github = directory("workflows", workflows, 0o040000);
        let tree_id = directory(".github", github, 0o040000);
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
        let workflows = list_ci_workflow_files(root.path(), &sha.to_string()).unwrap();
        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0].content, "on: push\n");
    }
}

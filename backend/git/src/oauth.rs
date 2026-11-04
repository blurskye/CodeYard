//! Credentials without the workflows permission may not change the workflows they publish.
use super::repository::{open_quarantined, peel_revision, tree_entry};
use anyhow::{ensure, Result};
use git2::{ErrorCode, ObjectType, Oid, Repository};
use std::{
    collections::{BTreeSet, HashSet},
    path::Path,
};

/// A workflow directory larger than this is refused rather than listed in server memory.
const MAX_WORKFLOW_TREE_BYTES: usize = 256 * 1024;
/// How many branch tips one check reads while looking for already published workflows.
const MAX_VOUCHING_BRANCHES: usize = 10_000;

type Entry = (Vec<u8>, Oid, i32);

/// The entries of `.github/workflows` at `sha`, as name, object and mode. A tag naming a
/// tree or blob publishes no workflow. Every tree on the way is size-checked before it is read.
fn workflows(repo: &Repository, sha: &str) -> Result<BTreeSet<Entry>> {
    if sha.bytes().all(|b| b == b'0') {
        return Ok(BTreeSet::new());
    }
    let (oid, ObjectType::Commit) = peel_revision(repo, sha)? else {
        return Ok(BTreeSet::new());
    };
    let commit = repo.find_commit(oid)?;
    let entry = match tree_entry(repo, &commit, ".github/workflows") {
        Ok(entry) => entry,
        Err(error)
            if error
                .downcast_ref::<git2::Error>()
                .is_some_and(|error| error.code() == ErrorCode::NotFound) =>
        {
            return Ok(BTreeSet::new())
        }
        Err(error) => return Err(error),
    };
    if entry.kind() != Some(ObjectType::Tree) {
        return Ok(BTreeSet::from([(Vec::new(), entry.id(), entry.filemode())]));
    }
    let (size, _) = repo.odb()?.read_header(entry.id())?;
    ensure!(
        size <= MAX_WORKFLOW_TREE_BYTES,
        "The workflow directory is too large to check"
    );
    Ok(repo
        .find_tree(entry.id())?
        .iter()
        .map(|entry| (entry.name_bytes().to_vec(), entry.id(), entry.filemode()))
        .collect())
}

/// Whether existing branches already have every entry in `wanted`. The scan stops once they
/// are all found, reads each distinct tip once, and past its budget answers no.
fn on_branches(repo: &Repository, wanted: &BTreeSet<Entry>) -> Result<bool> {
    let mut missing = wanted.clone();
    let mut seen = HashSet::new();
    for reference in repo.references_glob("refs/heads/*")? {
        let Some(tip) = reference?.target() else {
            continue;
        };
        if !seen.insert(tip) {
            continue;
        }
        if seen.len() > MAX_VOUCHING_BRANCHES {
            return Ok(false);
        }
        // A branch that does not name a readable commit vouches for nothing.
        if let Ok(found) = workflows(repo, &tip.to_string()) {
            missing.retain(|entry| !found.contains(entry));
            if missing.is_empty() {
                return Ok(true);
            }
        }
    }
    Ok(missing.is_empty())
}

/// Refuses `updates` (old and new commit of each ref) that add, change or delete a workflow
/// file, unless an existing branch already has that exact file, as GitHub allows.
pub fn check_workflows(
    path: &Path,
    quarantine: Option<&Path>,
    updates: &[(String, String)],
) -> Result<()> {
    let repo = open_quarantined(path, quarantine)?;
    let mut wanted = BTreeSet::new();
    for (old, new) in updates {
        if new.bytes().all(|b| b == b'0') {
            continue;
        }
        let before = workflows(&repo, old)?;
        let after = workflows(&repo, new)?;
        let deleted = before
            .iter()
            .any(|(name, ..)| !after.iter().any(|(kept, ..)| kept == name));
        ensure!(!deleted, WORKFLOW_DENIED);
        wanted.extend(after.difference(&before).cloned());
    }
    ensure!(
        wanted.is_empty() || on_branches(&repo, &wanted)?,
        WORKFLOW_DENIED
    );
    Ok(())
}

pub(crate) const WORKFLOW_DENIED: &str =
    "Refusing to create, update or delete workflow files without the workflows permission";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_workflow_scope_checks_published_trees() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(root.path()).unwrap();
        let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
        let empty = repo.treebuilder(None).unwrap().write().unwrap();
        let commit = |tree| {
            repo.commit(
                None,
                &signature,
                &signature,
                "test",
                &repo.find_tree(tree).unwrap(),
                &[],
            )
            .unwrap()
            .to_string()
        };
        let plain = commit(empty);
        let blob = repo.blob(b"on: push").unwrap();
        let mut workflow = repo.treebuilder(None).unwrap();
        workflow.insert("ci.yml", blob, 0o100644).unwrap();
        let mut github = repo.treebuilder(None).unwrap();
        github
            .insert("workflows", workflow.write().unwrap(), 0o040000)
            .unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert(".github", github.write().unwrap(), 0o040000)
            .unwrap();
        let workflow = commit(tree.write().unwrap());
        let zero = "0".repeat(40);
        assert!(check_workflows(root.path(), None, &[(zero.clone(), plain.clone())]).is_ok());
        assert!(check_workflows(root.path(), None, &[(plain.clone(), workflow.clone())]).is_err());
        assert!(
            check_workflows(root.path(), None, &[(workflow.clone(), workflow.clone())]).is_ok()
        );
        assert!(check_workflows(root.path(), None, &[(workflow.clone(), plain.clone())]).is_err());
        // A new branch may carry workflows that a branch already has, but not new ones.
        assert!(check_workflows(root.path(), None, &[(zero.clone(), workflow.clone())]).is_err());
        repo.reference(
            "refs/heads/main",
            Oid::from_str(&workflow).unwrap(),
            false,
            "test",
        )
        .unwrap();
        assert!(check_workflows(root.path(), None, &[(zero.clone(), workflow.clone())]).is_ok());
        assert!(check_workflows(root.path(), None, &[(zero.clone(), plain)]).is_ok());
        // A branch with only some of main's workflow files adds none.
        let release = repo.blob(b"on: release").unwrap();
        let mut both = repo.treebuilder(None).unwrap();
        both.insert("ci.yml", blob, 0o100644).unwrap();
        both.insert("release.yml", release, 0o100644).unwrap();
        let mut github = repo.treebuilder(None).unwrap();
        github
            .insert("workflows", both.write().unwrap(), 0o040000)
            .unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert(".github", github.write().unwrap(), 0o040000)
            .unwrap();
        let both = commit(tree.write().unwrap());
        assert!(check_workflows(root.path(), None, &[(zero.clone(), both.clone())]).is_err());
        repo.reference(
            "refs/heads/main",
            Oid::from_str(&both).unwrap(),
            true,
            "test",
        )
        .unwrap();
        assert!(check_workflows(root.path(), None, &[(zero.clone(), workflow.clone())]).is_ok());
        // An existing branch may also take files that a branch already has.
        assert!(check_workflows(root.path(), None, &[(workflow.clone(), both.clone())]).is_ok());
        assert!(check_workflows(root.path(), None, &[(both.clone(), workflow.clone())]).is_err());
        // A tag naming a blob publishes no workflow.
        assert!(check_workflows(root.path(), None, &[(zero.clone(), blob.to_string())]).is_ok());
        assert!(check_workflows(root.path(), None, &[(workflow, zero)]).is_ok());
    }
}

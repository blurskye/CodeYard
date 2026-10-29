//! OAuth pushes without workflow permission may not change the workflows they publish.
use super::repository::peel_to_commit_checked;
use anyhow::{ensure, Result};
use git2::{ErrorCode, ObjectType, Oid, Repository};
use std::{collections::BTreeSet, path::Path};

/// A workflow directory larger than this is refused rather than listed in server memory.
const MAX_WORKFLOW_TREE_BYTES: usize = 256 * 1024;
/// How many distinct workflow entries the branches may vouch for in one check.
const MAX_KNOWN_WORKFLOWS: usize = 100_000;

/// The entries of `.github/workflows` at commit `sha`, as name, object and mode.
fn workflows(repo: &Repository, sha: &str) -> Result<BTreeSet<(Vec<u8>, Oid, i32)>> {
    if sha.bytes().all(|b| b == b'0') {
        return Ok(BTreeSet::new());
    }
    let tree = peel_to_commit_checked(repo, sha)?.tree()?;
    let entry = match tree.get_path(Path::new(".github/workflows")) {
        Ok(entry) => entry,
        Err(error) if error.code() == ErrorCode::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error.into()),
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

/// The workflow entries that some branch already has.
fn on_branches(repo: &Repository) -> Result<BTreeSet<(Vec<u8>, Oid, i32)>> {
    let mut known = BTreeSet::new();
    for reference in repo.references_glob("refs/heads/*")? {
        let Some(tip) = reference?.target() else {
            continue;
        };
        // A branch that does not name a readable commit adds nothing it could vouch for.
        if let Ok(found) = workflows(repo, &tip.to_string()) {
            known.extend(found);
        }
        // Past the budget the rest are not consulted, so the push may be refused, never allowed.
        if known.len() >= MAX_KNOWN_WORKFLOWS {
            break;
        }
    }
    Ok(known)
}

pub(crate) fn check_workflows(
    path: &Path,
    quarantine: Option<&Path>,
    updates: &[(String, String)],
) -> Result<()> {
    let repo = Repository::open_bare(path)?;
    if let Some(quarantine) = quarantine {
        repo.odb()?.add_disk_alternate(
            quarantine
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid quarantine path"))?,
        )?;
    }
    let mut known = None;
    for (old, new) in updates {
        if new.bytes().all(|b| b == b'0') {
            continue;
        }
        let after = workflows(&repo, new)?;
        // A new branch changes no workflow file when a branch already has each of its files.
        let unchanged = if !old.bytes().all(|b| b == b'0') {
            workflows(&repo, old)? == after
        } else if after.is_empty() {
            true
        } else {
            if known.is_none() {
                known = Some(on_branches(&repo)?);
            }
            known.as_ref().is_some_and(|known| after.is_subset(known))
        };
        ensure!(
            unchanged,
            "OAuth token requires the workflow scope to update workflows"
        );
    }
    Ok(())
}

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
        assert!(check_workflows(root.path(), None, &[(workflow, zero)]).is_ok());
    }
}

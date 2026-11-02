//! Push protection reads the receive quarantine without publishing its objects.
use super::repository::{peel_revision, process, quarantined_command};
use crate::backend::{
    db::secret_scanning::Location,
    repositories::secret_scanning::{
        patterns::Pattern, scan_with_patterns, Detection, MAX_BLOB_BYTES,
    },
};
use anyhow::{ensure, Context, Result};
use git2::{ObjectType, Oid, Repository};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) struct Finding {
    pub detection: Detection,
    pub location: Location,
}

/// The time one push's scan may take.
const SCAN_TIME: Duration = Duration::from_secs(10);
/// The most bytes of changed-file listing a push may produce, about a hundred thousand files.
const MAX_LISTING_BYTES: usize = 16 * 1024 * 1024;
const BUDGET: &str = "Secret scanning exceeded its push budget; split this push";

fn open(path: &Path, quarantine: Option<&Path>) -> Result<Repository> {
    let repo = Repository::open_bare(path)?;
    if let Some(quarantine) = quarantine {
        repo.odb()?.add_disk_alternate(
            quarantine
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid quarantine path"))?,
        )?;
    }
    Ok(repo)
}

/// The commits that `heads` bring and no ref has yet.
fn new_commits(
    path: &Path,
    quarantine: Option<&Path>,
    heads: &[String],
    deadline: Instant,
) -> Result<Vec<Oid>> {
    let repo = open(path, quarantine)?;
    let mut walk = repo.revwalk()?;
    // Tags are peeled with the size checks of `peel_revision`, so a large blob is never loaded
    // just to learn that it is not a commit.
    for head in heads {
        let (oid, kind) = peel_revision(&repo, head)?;
        if kind == ObjectType::Commit {
            walk.push(oid)?;
        }
    }
    // Already reachable history cannot have been added by this push, including a new branch
    // starting at a known commit. Do not hide the quarantine's proposed tips, nor the objects
    // that the Git-data API keeps for replicas, which no scan has checked.
    for reference in repo.references()? {
        let reference = reference?;
        if let Some(name) = reference
            .name()
            .filter(|name| !name.starts_with("refs/codeyard/objects/"))
        {
            if let Ok((oid, ObjectType::Commit)) = peel_revision(&repo, name) {
                walk.hide(oid)?;
            }
        }
    }
    let mut commits = Vec::new();
    for oid in walk {
        ensure!(Instant::now() < deadline, BUDGET);
        commits.push(oid?);
    }
    Ok(commits)
}

/// The secrets that the commits `heads` bring add, each compared with the file it replaces.
pub(crate) async fn added_secrets(
    path: PathBuf,
    quarantine: Option<PathBuf>,
    heads: Vec<String>,
    patterns: Vec<Pattern>,
    disabled: Vec<String>,
) -> Result<Vec<Finding>> {
    let deadline = Instant::now() + SCAN_TIME;
    let commits = {
        let (path, quarantine) = (path.clone(), quarantine.clone());
        tokio::task::spawn_blocking(move || {
            new_commits(&path, quarantine.as_deref(), &heads, deadline)
        })
        .await??
    };
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    // A Git child with a memory limit lists the changed files, so a push of millions of files
    // is refused by the listing's bound instead of filling the server's memory.
    let mut command = quarantined_command(&path, quarantine.as_deref())?;
    command.args([
        "diff-tree",
        "--stdin",
        "-r",
        "-z",
        "--raw",
        "--no-renames",
        "--root",
        "--diff-merges=first-parent",
        "--no-abbrev",
    ]);
    process::limit_memory(&mut command);
    let input: String = commits.iter().map(|id| format!("{id}\n")).collect();
    let (status, listing) = process::buffered(
        command,
        input.as_bytes(),
        MAX_LISTING_BYTES,
        deadline.saturating_duration_since(Instant::now()),
    )
    .await
    .context(BUDGET)?;
    ensure!(status.success(), "Git could not list the pushed changes");
    tokio::task::spawn_blocking(move || {
        scan(
            &path,
            quarantine.as_deref(),
            &listing,
            &patterns,
            &disabled,
            deadline,
        )
    })
    .await?
}

/// Scans the blobs that `listing`, the output of `git diff-tree --stdin -r -z --raw`, adds.
fn scan(
    path: &Path,
    quarantine: Option<&Path>,
    listing: &[u8],
    patterns: &[Pattern],
    disabled: &[String],
    deadline: Instant,
) -> Result<Vec<Finding>> {
    let repo = open(path, quarantine)?;
    // Object headers give blob sizes without reading a large blob into memory.
    let odb = repo.odb()?;
    let small = |id: Oid| {
        odb.read_header(id)
            .is_ok_and(|(size, _)| size <= MAX_BLOB_BYTES)
    };
    let mut found = Vec::new();
    let mut bytes = 0usize;
    let mut commit = String::new();
    let mut fields = listing.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        // A field that is not a change names the commit whose changes follow.
        let Some(change) = field.strip_prefix(b":") else {
            if !field.is_empty() {
                commit = String::from_utf8_lossy(field).into_owned();
            }
            continue;
        };
        let file = fields
            .next()
            .context("Git listed a change without a path")?;
        // ":<old mode> <new mode> <old id> <new id> <status>"
        let change: Vec<&str> = std::str::from_utf8(change)?.split(' ').collect();
        ensure!(change.len() == 5, "Git listed a malformed change");
        let (mode, old, new) = (
            change[1],
            Oid::from_str(change[2])?,
            Oid::from_str(change[3])?,
        );
        // A deletion adds nothing, and a gitlink names a commit, never a blob.
        if new.is_zero() || mode == "160000" || !small(new) {
            continue;
        }
        let blob = repo.find_blob(new)?;
        bytes += blob.size();
        ensure!(
            bytes <= 64 * MAX_BLOB_BYTES && Instant::now() < deadline,
            BUDGET
        );
        let previous = if !old.is_zero() && small(old) {
            repo.find_blob(old).ok()
        } else {
            None
        };
        // A name that is not UTF-8 still belongs to a blob that must be scanned.
        let path = String::from_utf8_lossy(file).into_owned();
        // Only a secret the same rules found in the replaced file already existed; bytes the
        // scanner skipped, such as a binary file's, were never checked.
        let existing: Vec<String> = match previous {
            Some(old) => scan_with_patterns(&path, &commit, old.content(), patterns, true)?
                .into_iter()
                .map(|detection| detection.secret)
                .collect(),
            None => Vec::new(),
        };
        for detection in scan_with_patterns(&path, &commit, blob.content(), patterns, true)? {
            if disabled.contains(&detection.secret_type) || existing.contains(&detection.secret) {
                continue;
            }
            found.push(Finding {
                location: Location {
                    commit_sha: commit.clone(),
                    blob_sha: new.to_string(),
                    path: path.clone(),
                    start_line: detection.start_line,
                    end_line: detection.end_line,
                    start_column: detection.start_column,
                    end_column: detection.end_column,
                },
                detection,
            });
            ensure!(
                found.len() <= 20,
                "Push contains more than 20 secrets; remove them and try again"
            );
        }
    }
    Ok(found)
}

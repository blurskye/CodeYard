//! Push protection reads the receive quarantine without publishing its objects.
use super::repository::{open_quarantined, peel_revision, process, quarantined_command, walk_commits};
use crate::backend::{
    db::secret_scanning::Location,
    repositories::secret_scanning::{
        patterns::Pattern, scan_with_patterns_until, Detection, MAX_BLOB_BYTES,
    },
};
use anyhow::{ensure, Context, Result};
use git2::{ObjectType, Oid};
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

/// The commits that `heads` bring and no ref has yet.
async fn new_commits(
    path: PathBuf,
    quarantine: Option<PathBuf>,
    heads: Vec<String>,
    deadline: Instant,
) -> Result<Vec<Oid>> {
    let (read_path, read_quarantine) = (path.clone(), quarantine.clone());
    let tips = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        let repo = open_quarantined(&read_path, read_quarantine.as_deref())?;
        let mut tips = Vec::new();
        for head in heads {
            ensure!(Instant::now() < deadline, BUDGET);
            let (oid, kind) = peel_revision(&repo, &head)?;
            if kind == ObjectType::Commit {
                tips.push(oid.to_string());
            }
        }
        Ok(tips)
    }).await??;
    if tips.is_empty() {
        return Ok(Vec::new());
    }
    // Pull heads and retention refs have not passed push protection. Only published
    // branches, tags and notes can establish the boundary of previously accepted history.
    walk_commits(&path, quarantine.as_deref(), &tips, &[], true, 10_000,
        deadline.saturating_duration_since(Instant::now())).await
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
    let commits = new_commits(path.clone(), quarantine.clone(), heads, deadline).await?;
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
    let repo = open_quarantined(path, quarantine)?;
    // Object headers give blob sizes without reading a large blob into memory.
    let odb = repo.odb()?;
    let small = |id: Oid| {
        odb.read_header(id).map(|(size, _)| size <= MAX_BLOB_BYTES)
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
        if new.is_zero() || mode == "160000" || !small(new)? {
            continue;
        }
        let blob = repo.find_blob(new)?;
        bytes += blob.size();
        ensure!(
            bytes <= 64 * MAX_BLOB_BYTES && Instant::now() < deadline,
            BUDGET
        );
        let previous = if !old.is_zero() && small(old)? {
            repo.find_blob(old).ok()
        } else {
            None
        };
        // A name that is not UTF-8 still belongs to a blob that must be scanned.
        let path = String::from_utf8_lossy(file).into_owned();
        // Only a secret the same rules found in the replaced file already existed; bytes the
        // scanner skipped, such as a binary file's, were never checked.
        let replacement = scan_with_patterns_until(&path, &commit, blob.content(), patterns, true, deadline)?;
        let replacement: Vec<_> = replacement.into_iter().filter(|detection| !disabled.contains(&detection.secret_type)).collect();
        if replacement.is_empty() {
            continue;
        }
        let existing: Vec<(String, String)> = match previous {
            Some(old) => scan_with_patterns_until(&path, &commit, old.content(), patterns, true, deadline)?
                .into_iter()
                .filter(|detection| !disabled.contains(&detection.secret_type))
                .map(|detection| (detection.secret_type, detection.secret))
                .collect(),
            None => Vec::new(),
        };
        for detection in replacement {
            if existing.iter().any(|(kind, value)| kind == &detection.secret_type && value == &detection.secret)
                || found.iter().any(|finding: &Finding| finding.detection.secret_type == detection.secret_type && finding.detection.secret == detection.secret)
            {
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

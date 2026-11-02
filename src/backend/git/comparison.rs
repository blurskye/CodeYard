//! Comparison metadata and oldest-first pages without copying objects between forks.
use super::{
    repository::{comparison_repository, process, COMMIT_WALK_TIMEOUT},
    CommitInfo, ObjectTooLarge, MAX_COMMIT_PAGE_BYTES,
};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Instant};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitComparison {
    pub base_commit: CommitInfo,
    pub merge_base_commit: CommitInfo,
    pub head_sha: String,
    pub ahead_by: usize,
    pub behind_by: usize,
    pub commits: Vec<CommitInfo>,
}

pub(crate) async fn compare_commits(
    base_path: &Path,
    head_path: &Path,
    base_ref: &str,
    head_ref: &str,
    offset: usize,
    limit: usize,
) -> Result<CommitComparison> {
    let deadline = Instant::now() + COMMIT_WALK_TIMEOUT;
    let (base_path, head_path, base_ref, head_ref) = (
        base_path.to_owned(),
        head_path.to_owned(),
        base_ref.to_owned(),
        head_ref.to_owned(),
    );
    let (prepare_base, prepare_head) = (base_path.clone(), head_path.clone());
    let (temporary, base, head) = process::blocking_until(deadline, move || {
        comparison_repository(&prepare_base, &prepare_head, &base_ref, &head_ref)
    })
    .await?;
    let path = temporary.path();
    let mut command = process::git_command()?;
    command
        .args([
            "rev-list",
            "--left-right",
            "--count",
            &format!("{base}...{head}"),
            "--",
        ])
        .current_dir(path);
    let (status, output) = process::buffered(command, b"", 64, COMMIT_WALK_TIMEOUT).await?;
    ensure!(status.success(), "Git comparison count failed");
    let counts = std::str::from_utf8(&output)?
        .split_whitespace()
        .map(str::parse::<usize>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(counts.len() == 2, "Git returned invalid comparison counts");
    let (behind_by, ahead_by) = (counts[0], counts[1]);
    let mut command = process::git_command()?;
    command
        .args(["merge-base", &base.to_string(), &head.to_string()])
        .current_dir(path);
    let (status, output) = process::buffered(
        command,
        b"",
        64,
        deadline.saturating_duration_since(Instant::now()),
    )
    .await?;
    if !status.success() {
        if status.code() == Some(1) {
            return Err(git2::Error::new(
                git2::ErrorCode::NotFound,
                git2::ErrorClass::Object,
                "No common ancestor",
            )
            .into());
        }
        anyhow::bail!("Git merge base lookup failed");
    }
    let merge_base = git2::Oid::from_str(std::str::from_utf8(&output)?.trim())?;
    let mut ids = Vec::new();
    let count = ahead_by.saturating_sub(offset).min(limit);
    if count > 0 {
        // Git applies --reverse after limiting. Select the corresponding newest-first
        // window first, then reverse that window so pagination remains oldest first.
        let skip = ahead_by.saturating_sub(offset.saturating_add(count));
        let mut command = process::git_command()?;
        command
            .args([
                "rev-list",
                "--reverse",
                &format!("--skip={skip}"),
                &format!("--max-count={count}"),
                &format!("{base}..{head}"),
                "--",
            ])
            .current_dir(path);
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow::anyhow!("Git comparison timed out"))?;
        let (status, output) = process::buffered(command, b"", count * 41, remaining).await?;
        ensure!(status.success(), "Git comparison walk failed");
        for line in output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            ids.push(git2::Oid::from_str(std::str::from_utf8(line)?)?);
        }
    }
    process::blocking_until(deadline, move || {
        let repo = git2::Repository::open_bare(temporary.path())?;
        let odb = repo.odb()?;
        // Git accepts quoted alternates in its file, but libgit2 does not unquote them.
        // Install the paths explicitly on this handle before reading metadata.
        for source in [base_path, head_path] {
            let source = git2::Repository::open_bare(source)?;
            let objects = source.path().join("objects").canonicalize()?;
            odb.add_disk_alternate(
                objects
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("Invalid object storage path"))?,
            )?;
        }
        let metadata =
            |id| super::repository::commit_info(&repo, &super::find_commit_checked(&repo, id)?);
        let mut bytes = 0usize;
        let mut commits = Vec::new();
        for id in ids {
            ensure!(Instant::now() < deadline, "Git comparison timed out");
            bytes = bytes
                .checked_add(odb.read_header(id)?.0)
                .filter(|bytes| *bytes <= MAX_COMMIT_PAGE_BYTES)
                .ok_or(ObjectTooLarge)?;
            commits.push(metadata(id)?);
        }
        Ok(CommitComparison {
            base_commit: metadata(base)?,
            merge_base_commit: metadata(merge_base)?,
            head_sha: head.to_string(),
            ahead_by,
            behind_by,
            commits,
        })
    })
    .await
}

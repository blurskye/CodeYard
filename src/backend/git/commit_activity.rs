//! Commit activity on one revision: who committed when, and how many lines each commit changed.
//! Insights reads it. Newest commits come first, and the read stops at `MAX_ACTIVITY_COMMITS`.
use super::repository::{peel_to_commit_checked, process};
use super::stats::is_not_found;
use anyhow::{anyhow, Result};
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The newest commits an activity read covers. Older history is not summarized.
pub const MAX_ACTIVITY_COMMITS: usize = 5_000;
/// Output above this size is refused rather than parsed; the page says the history is too large.
const MAX_ACTIVITY_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

const RECORD_SEPARATOR: char = '\u{1e}';
const FIELD_SEPARATOR: char = '\u{1f}';

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitActivity {
    pub author_name: String,
    pub author_email: String,
    /// The author time, in Unix seconds.
    pub authored_at: i64,
    pub files: u32,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitActivityLog {
    /// Newest first.
    pub commits: Vec<CommitActivity>,
    /// The revision has more commits than `commits` holds.
    pub truncated: bool,
}

fn parse_commit_activity(output: &[u8]) -> Result<CommitActivityLog> {
    let text = String::from_utf8_lossy(output);
    let mut commits = Vec::new();
    // The text before the first separator is empty.
    for record in text.split(RECORD_SEPARATOR).skip(1) {
        let mut lines = record.lines();
        let header = lines
            .next()
            .ok_or_else(|| anyhow!("Malformed Git activity record"))?;
        let mut fields = header.splitn(3, FIELD_SEPARATOR);
        let (Some(name), Some(email), Some(time)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(anyhow!("Malformed Git activity record"));
        };
        let mut commit = CommitActivity {
            author_name: name.to_owned(),
            author_email: email.to_owned(),
            authored_at: time.trim().parse()?,
            files: 0,
            additions: 0,
            deletions: 0,
        };
        for line in lines.filter(|line| !line.is_empty()) {
            let mut parts = line.splitn(3, '\t');
            let (Some(added), Some(removed), Some(_path)) =
                (parts.next(), parts.next(), parts.next())
            else {
                return Err(anyhow!("Malformed Git numstat record"));
            };
            commit.files += 1;
            // Binary files report "-" for both counts; they add no lines.
            commit.additions += added.parse::<u64>().unwrap_or(0);
            commit.deletions += removed.parse::<u64>().unwrap_or(0);
        }
        commits.push(commit);
    }
    let truncated = commits.len() > MAX_ACTIVITY_COMMITS;
    commits.truncate(MAX_ACTIVITY_COMMITS);
    Ok(CommitActivityLog { commits, truncated })
}

/// The commit activity of `git_ref`. An unborn branch has no activity.
pub(crate) async fn read_commit_activity(
    repo_path: &Path,
    git_ref: &str,
) -> Result<CommitActivityLog> {
    let repo = Repository::open_bare(repo_path)?;
    let revision = match peel_to_commit_checked(&repo, git_ref) {
        Ok(commit) => commit.id().to_string(),
        Err(error) if is_not_found(&error) => return Ok(CommitActivityLog::default()),
        Err(error) => return Err(error),
    };
    drop(repo);
    let mut command = process::git_command()?;
    command
        .args([
            "log",
            "--no-color",
            "--numstat",
            "--format=%x1e%an%x1f%ae%x1f%at",
            "-n",
            &(MAX_ACTIVITY_COMMITS + 1).to_string(),
            &revision,
            "--",
        ])
        .current_dir(repo_path);
    let (status, output) = process::buffered(
        command,
        b"",
        MAX_ACTIVITY_OUTPUT_BYTES,
        process::RPC_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(status.success(), "Git activity listing failed");
    parse_commit_activity(&output)
}

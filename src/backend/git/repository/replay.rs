//! Replays the commits of one history onto another, one at a time, the way a rebase does.
use super::checked::{command, commit_tree, merge_tree_result, tree_of, Person, UnsupportedMerge};
use super::validate_merge_revision;
use super::MAX_COMMIT_OBJECT_BYTES;
use anyhow::{bail, ensure, Context, Result};
use std::{path::Path, time::Duration};

/// The most time one replay may take, across all of its Git commands together.
const REPLAY_DEADLINE: Duration = Duration::from_secs(120);

/// The most commits one replay may cover. A longer history is refused.
const MAX_REPLAYED_COMMITS: usize = 100;

/// What a replay produced: the new tip, and how many commits were written. A commit whose
/// change the tip already has is skipped, so the rest are the ones it did not write.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Replayed {
    pub(crate) tip: String,
    pub(crate) replayed: usize,
}

struct PendingCommit {
    id: String,
    parent: String,
}

struct OriginalCommit {
    name: String,
    email: String,
    date: String,
    message: Vec<u8>,
}

/// Replays the commits of `from..to`, oldest first, on top of `onto`. Each commit's change is
/// merged onto the tip against its own parent. The new commit keeps the author, author date and
/// message of the original, and `committer` commits it. A commit whose tree equals the tip's tree
/// is skipped. Merge commits, parentless commits and more than 100 commits are refused. A
/// conflict is a `MergeConflict` that names the conflicting paths.
pub(crate) async fn replay_commits(
    path: &Path,
    from: &str,
    to: &str,
    onto: &str,
    committer: &Person<'_>,
) -> Result<Replayed> {
    // Dropping the replay when the deadline passes kills the Git command that is running.
    tokio::time::timeout(REPLAY_DEADLINE, replay(path, from, to, onto, committer))
        .await
        .context("git replay timed out")?
}

async fn replay(
    path: &Path,
    from: &str,
    to: &str,
    onto: &str,
    committer: &Person<'_>,
) -> Result<Replayed> {
    let commits = commits_to_replay(path, from, to).await?;
    let mut tip = onto.to_owned();
    let mut tip_tree = tree_of(path, onto).await?;
    let mut replayed = 0;
    for commit in &commits {
        let merge_base = format!("--merge-base={}", commit.parent);
        let (status, output) = command(
            path,
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                &merge_base,
                tip.as_str(),
                commit.id.as_str(),
            ],
            b"",
            None,
        )
        .await?;
        let tree = merge_tree_result(status, &output)?;
        if tree == tip_tree {
            continue;
        }
        let original = original_commit(path, &commit.id).await?;
        let author = Person {
            name: &original.name,
            email: &original.email,
            date: Some(&original.date),
        };
        let parents = [tip.as_str()];
        tip = commit_tree(path, &tree, &parents, &original.message, &author, committer).await?;
        ensure_commit_fits(path, &tip).await?;
        tip_tree = tree;
        replayed += 1;
    }
    Ok(Replayed { tip, replayed })
}

/// Refuses a replayed commit that Git would not read back. A replayed commit keeps its message but
/// takes the merger as committer, whose line may be longer than the original one, so its object can
/// grow past `MAX_COMMIT_OBJECT_BYTES`. The size is what the reads measure, and it is measured here
/// before anything is published.
async fn ensure_commit_fits(path: &Path, commit: &str) -> Result<()> {
    let (status, output) = command(path, &["cat-file", "-s", commit], b"", None).await?;
    ensure!(status.success(), "Git could not measure a replayed commit");
    let size: usize = std::str::from_utf8(&output)?.trim().parse()?;
    if size > MAX_COMMIT_OBJECT_BYTES {
        return Err(
            UnsupportedMerge("A commit would be too large after rebasing.".to_owned()).into(),
        );
    }
    Ok(())
}

/// The commits of `from..to`, oldest first. Only commits with exactly one parent can be replayed.
async fn commits_to_replay(path: &Path, from: &str, to: &str) -> Result<Vec<PendingCommit>> {
    validate_merge_revision("main", from)?;
    validate_merge_revision("main", to)?;
    let range = format!("{from}..{to}");
    // One more than the limit is enough to know that the history is too long.
    let limit = format!("--max-count={}", MAX_REPLAYED_COMMITS + 1);
    let (status, output) = command(
        path,
        &["rev-list", "--parents", "--topo-order", &limit, &range],
        b"",
        None,
    )
    .await?;
    ensure!(status.success(), "Git could not list the commits to replay");
    let lines: Vec<&str> = std::str::from_utf8(&output)?.lines().collect();
    if lines.len() > MAX_REPLAYED_COMMITS {
        return Err(UnsupportedMerge(format!(
            "Rebase merging supports at most {MAX_REPLAYED_COMMITS} commits."
        ))
        .into());
    }
    let mut commits = Vec::with_capacity(lines.len());
    for line in lines {
        match line.split_whitespace().collect::<Vec<_>>().as_slice() {
            [id, parent] => commits.push(PendingCommit {
                id: (*id).to_owned(),
                parent: (*parent).to_owned(),
            }),
            [_] => {
                return Err(UnsupportedMerge(
                    "Rebase merging cannot replay a commit without a parent.".to_owned(),
                )
                .into())
            }
            _ => {
                return Err(UnsupportedMerge(
                    "Rebase merging cannot replay merge commits.".to_owned(),
                )
                .into())
            }
        }
    }
    commits.reverse();
    Ok(commits)
}

/// Who wrote `commit`, when, and its message, as Git reads them.
async fn original_commit(path: &Path, commit: &str) -> Result<OriginalCommit> {
    let (status, output) = command(
        path,
        &[
            "log",
            "-1",
            "--date=raw",
            "--pretty=format:%an%x00%ae%x00%ad%x00%B",
            commit,
        ],
        b"",
        None,
    )
    .await?;
    ensure!(status.success(), "Git could not read a commit to replay");
    let fields: Vec<&[u8]> = output.splitn(4, |byte| *byte == 0).collect();
    let [name, email, date, message] = fields.as_slice() else {
        bail!("Git returned an unreadable commit");
    };
    Ok(OriginalCommit {
        name: std::str::from_utf8(name)?.to_owned(),
        email: std::str::from_utf8(email)?.to_owned(),
        date: std::str::from_utf8(date)?.to_owned(),
        message: message.to_vec(),
    })
}

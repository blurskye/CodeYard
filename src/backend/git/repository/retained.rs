//! Hidden Git-data refs retain only objects not already reachable from ordinary refs.
use super::{process, Repository, MAX_RETAINED_OBJECT_REFS};
use anyhow::{ensure, Context, Result};
use std::{collections::HashMap, path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

pub(crate) fn exists(path: &Path, name: &str) -> Result<bool> {
    ensure!(name.starts_with("refs/codeyard/objects/"), "Invalid retention ref");
    let repo = Repository::open_bare(path)?;
    let result = match repo.find_reference(name) {
        Ok(_) => Ok(true),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    };
    result
}

/// One bounded batch also permits cleanup of repositories already above the admission limit.
pub(crate) fn candidates(path: &Path) -> Result<HashMap<String, String>> {
    let repo = Repository::open_bare(path)?;
    let mut refs = HashMap::new();
    for reference in repo.references_glob("refs/codeyard/objects/*")?.take(MAX_RETAINED_OBJECT_REFS) {
        let reference = reference?;
        if let (Some(name), Some(oid)) = (reference.name(), reference.target()) {
            if name == format!("refs/codeyard/objects/{oid}") {
                refs.insert(oid.to_string(), name.to_owned());
            }
        }
    }
    Ok(refs)
}

/// The caller holds the publication lease until the resulting CAS deletions finish.
/// Keep only proven matches if the bounded reachability walk does not finish.
pub(crate) async fn reachable(
    path: &Path, mut candidates: HashMap<String, String>,
) -> Result<Vec<(String, String)>> {
    if candidates.is_empty() { return Ok(Vec::new()); }
    let timeout = Duration::from_secs(5);
    let _permit = process::operation_slot(timeout).await?;
    let mut command = process::git_command()?;
    command.args(["rev-list", "--objects", "--no-object-names", "--branches", "--tags", "--glob=refs/pull/*"])
        .current_dir(path);
    process::limit_memory(&mut command);
    let mut child = command.spawn().context("start retained-object reachability walk")?;
    let mut group = process::ProcessGroup::new(child.id(), std::time::Duration::ZERO);
    let mut lines = BufReader::new(child.stdout.take().context("git reachability stdout")?).lines();
    let stderr = child.stderr.take().context("git reachability stderr")?;
    drop(child.stdin.take());
    let mut found = Vec::new();
    let walk = async {
        while let Some(oid) = lines.next_line().await? {
            ensure!(oid.len() == 40 && oid.bytes().all(|b| b.is_ascii_hexdigit()), "Invalid reachable object ID");
            if let Some(name) = candidates.remove(&oid) { found.push((name, oid)); }
            if candidates.is_empty() { return Ok::<_, anyhow::Error>(false); }
        }
        Ok(true)
    };
    let complete = match tokio::time::timeout(timeout, walk).await {
        Ok(result) => result?,
        Err(_) => false,
    };
    if !complete { process::kill_group(child.id()); }
    let status = child.wait().await?;
    let errors = process::read_bounded(stderr, 16 * 1024).await?;
    if complete { ensure!(status.success(), "Git reachability failed: {}", String::from_utf8_lossy(&errors)); }
    group.disarm();
    Ok(found)
}

pub(crate) async fn delete(path: &Path, refs: &[(String, String)]) -> Result<()> {
    if refs.is_empty() { return Ok(()); }
    ensure!(refs.len() <= MAX_RETAINED_OBJECT_REFS, "Retention cleanup exceeded its bound");
    let mut input = String::from("start\n");
    for (name, oid) in refs {
        ensure!(name == &format!("refs/codeyard/objects/{oid}") && git2::Oid::from_str(oid).is_ok(), "Invalid retention cleanup");
        input.push_str(&format!("delete {name} {oid}\n"));
    }
    input.push_str("prepare\ncommit\n");
    let mut command = process::git_command()?;
    command.args(["update-ref", "--stdin"]).current_dir(path);
    process::limit_memory(&mut command);
    let result = process::run(command, input.as_bytes(), 1024, Duration::from_secs(30)).await?;
    ensure!(result.status.success(), "Retention cleanup failed: {}", String::from_utf8_lossy(&result.stderr));
    Ok(())
}

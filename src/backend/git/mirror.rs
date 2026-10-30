//! Whole-repository copies for backups and restores, the Git checks and repairs that follow a
//! restore, and the `deleted/` directory that keeps deleted repositories for a while.
use super::repository::process;
use anyhow::{ensure, Context, Result};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::process::Command;

/// Kept deleted repositories live here, under the repository root.
pub(crate) const DELETED_DIR: &str = "deleted";
const TIMEOUT: Duration = Duration::from_secs(3600);
const OUTPUT_LIMIT: usize = 64 * 1024 * 1024;

fn unix_seconds() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

/// Moves a deleted repository to `<parent>/deleted/<unix seconds>-<name>`. A repository that
/// is already gone counts as moved.
pub(crate) fn retire(path: &Path) -> Result<()> {
    if let Err(error) = std::fs::symlink_metadata(path) {
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(error.into());
    }
    let parent = path
        .parent()
        .context("repository has no parent directory")?;
    let name = path.file_name().context("repository has no name")?;
    let kept = parent.join(DELETED_DIR);
    std::fs::create_dir_all(&kept)?;
    let target = kept.join(format!("{}-{}", unix_seconds()?, name.to_string_lossy()));
    std::fs::rename(path, &target)?;
    std::fs::File::open(parent)?.sync_all()?;
    std::fs::File::open(&kept)?.sync_all()?;
    Ok(())
}

/// The kept deleted repositories under `root`, as (deleted at, original name, path).
fn deleted_entries(root: &Path) -> Result<Vec<(u64, String, PathBuf)>> {
    let entries = match std::fs::read_dir(root.join(DELETED_DIR)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut kept = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file = entry.file_name();
        let Some((seconds, name)) = file.to_str().and_then(|file| file.split_once('-')) else {
            continue;
        };
        if let Ok(seconds) = seconds.parse() {
            kept.push((seconds, name.to_owned(), entry.path()));
        }
    }
    Ok(kept)
}

/// Index the newest retained copy of each repository in one root-directory scan.
pub(crate) fn deleted_copies(root: &Path) -> Result<std::collections::HashMap<String, PathBuf>> {
    let mut copies = std::collections::HashMap::<String, (u64, PathBuf)>::new();
    for (seconds, name, path) in deleted_entries(root)? {
        if copies.get(&name).is_none_or(|(previous, _)| seconds >= *previous) {
            copies.insert(name, (seconds, path));
        }
    }
    Ok(copies.into_iter().map(|(name, (_, path))| (name, path)).collect())
}

/// Removes the kept deleted repositories under `root` older than `window`.
pub(crate) fn remove_expired_deleted(root: &Path, window: Duration) -> Result<usize> {
    let now = unix_seconds()?;
    let mut removed = 0;
    let mut failed = 0;
    for (seconds, _, path) in deleted_entries(root)? {
        if now.saturating_sub(seconds) > window.as_secs() {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => removed += 1,
                Err(error) => {
                    failed += 1;
                    tracing::warn!(%error, "Could not expire retained repository");
                }
            }
        }
    }
    ensure!(failed == 0, "Could not expire {failed} retained repositories; removed {removed}");
    Ok(removed)
}

fn git(path: &Path) -> Result<Command> {
    let mut command = process::git_command()?;
    command.arg("--git-dir").arg(path);
    Ok(command)
}

async fn run(command: Command, input: &[u8]) -> Result<Vec<u8>> {
    let output = process::run(command, input, OUTPUT_LIMIT, TIMEOUT).await?;
    ensure!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

/// How many times a copy fetches again because HEAD moved to a branch it had not fetched.
const HEAD_ATTEMPTS: usize = 3;

/// Brings the bare repository at `copy` up to date with `source`: every ref, including the
/// hidden `refs/codeyard/` ones, HEAD, and every object file, so commits that no ref reaches
/// (a CI run may still name one) stay available. Only new objects travel, so copying into the
/// same place again is quick.
pub(crate) async fn mirror(source: &Path, copy: &Path) -> Result<()> {
    let source = std::path::absolute(source)?;
    if !copy.join("HEAD").is_file() {
        std::fs::create_dir_all(copy)?;
        let mut init = process::git_command()?;
        init.args(["init", "--quiet", "--bare"]).arg(copy);
        run(init, b"").await?;
    }
    // The object files go first. The fetch then finds the objects its refs need already there
    // and writes no second pack of them.
    let (objects, copied) = (source.join("objects"), copy.join("objects"));
    tokio::task::spawn_blocking(move || copy_missing_objects(&objects, &copied)).await??;
    for attempt in 1..=HEAD_ATTEMPTS {
        let mut fetch = git(copy)?;
        // A later `-c` overrides the default in `process::std_command`, which hides refs/codeyard/.
        fetch
            .args([
                "-c",
                "transfer.hideRefs=!refs/codeyard/",
                "fetch",
                "--quiet",
            ])
            .args(["--prune", "--no-write-fetch-head", "--"])
            .arg(&source)
            .arg("+refs/*:refs/*");
        run(fetch, b"").await?;
        let mut head = tempfile::NamedTempFile::new_in(copy)?;
        let mut source_head = std::fs::File::open(source.join("HEAD"))?;
        std::io::copy(&mut source_head, &mut head)?;
        head.as_file().sync_all()?;
        head.persist(copy.join("HEAD"))?;
        std::fs::File::open(copy)?.sync_all()?;
        // HEAD may name a branch that was created after the refs were fetched.
        if head_resolves(copy).await? || !head_resolves(&source).await? {
            break;
        }
        ensure!(
            attempt < HEAD_ATTEMPTS,
            "the default branch moved while the repository was copied"
        );
    }
    let mut prune = git(copy)?;
    prune.arg("prune-packed");
    run(prune, b"").await?;
    std::fs::File::open(copy.join("HEAD"))?.sync_all()?;
    std::fs::File::open(copy)?.sync_all()?;
    Ok(())
}

/// Whether HEAD names a commit in the repository at `path`.
async fn head_resolves(path: &Path) -> Result<bool> {
    let mut command = git(path)?;
    command.args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]);
    let output = process::run(command, b"", OUTPUT_LIMIT, TIMEOUT).await?;
    if output.status.success() { return Ok(true); }
    ensure!(output.status.code() == Some(1), "Git could not resolve backup HEAD");
    let mut symbolic = git(path)?;
    symbolic.args(["symbolic-ref", "-q", "HEAD"]);
    let reference = run(symbolic, b"").await?;
    let reference = std::str::from_utf8(&reference)?.strip_suffix('\n').context("Invalid symbolic HEAD")?;
    let mut exists = git(path)?;
    exists.args(["show-ref", "--verify", "--quiet", reference]);
    let output = process::run(exists, b"", OUTPUT_LIMIT, TIMEOUT).await?;
    ensure!(output.status.code() == Some(1), "Backup HEAD is not a verified unborn branch");
    Ok(false)
}

/// How many times the pack directory is copied again because repacking added a pack.
const PACK_ATTEMPTS: usize = 4;

/// Copies the object files that `copy` lacks. Objects are named by their content, so a file
/// that is already there is kept. Loose objects go first: repacking writes the new pack before
/// it deletes the loose objects it packed, so an object that leaves its loose directory is in a
/// pack by then. The pack directory is then copied until a pass finds nothing new, so a pack
/// that replaced one being copied is not missed.
fn copy_missing_objects(objects: &Path, copy: &Path) -> Result<()> {
    std::fs::create_dir_all(copy)?;
    if let Some(parent) = copy.parent() { std::fs::File::open(parent)?.sync_all()?; }
    for entry in std::fs::read_dir(objects)? {
        let entry = entry?;
        let name = entry.file_name();
        // Loose objects sit in two-hex-digit directories. The others hold push quarantines,
        // packs and files that are not named by their content.
        if name.len() == 2 && entry.file_type()?.is_dir() {
            copy_new_files(&entry.path(), &copy.join(&name))?;
        }
    }
    for _ in 0..PACK_ATTEMPTS {
        if !copy_new_files(&objects.join("pack"), &copy.join("pack"))? {
            // Incremental backups retain old packs until rotation: the previous database
            // remains authoritative if a later step of this backup fails.
            return Ok(());
        }
    }
    anyhow::bail!("the packs kept changing while the repository was copied")
}

/// Copies the files of `from` that `to` lacks, and says whether it copied any or saw one vanish,
/// since a vanished pack was replaced by one the listing may have missed. Temporary files are
/// skipped, and so is the multi-pack index, whose name does not follow its content.
fn copy_new_files(from: &Path, to: &Path) -> Result<bool> {
    let entries = match std::fs::read_dir(from) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    std::fs::create_dir_all(to)?;
    if let Some(parent) = to.parent() { std::fs::File::open(parent)?.sync_all()?; }
    let mut changed = false;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let text = name.to_string_lossy().into_owned();
        let target = to.join(&name);
        if text.starts_with("tmp_")
            || text.starts_with(".tmp-")
            || text.starts_with("multi-pack-index")
            || text.ends_with(".partial")
            || !entry.file_type()?.is_file()
            || target.exists()
        {
            continue;
        }
        let partial = to.join(format!("{text}.partial"));
        match std::fs::copy(entry.path(), &partial) {
            Ok(_) => {}
            // Repacking removed it after writing the pack that now holds its objects.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                changed = true;
                continue;
            }
            Err(error) => return Err(error.into()),
        }
        std::fs::File::open(&partial)?.sync_all()?;
        std::fs::rename(&partial, &target)?;
        changed = true;
    }
    std::fs::File::open(to)?.sync_all()?;
    Ok(changed)
}

/// What `git fsck` reports about the repository's refs and the objects they reach, or `None`
/// when it finds nothing.
pub(crate) async fn check(path: &Path) -> Result<Option<String>> {
    let mut command = git(path)?;
    command.args([
        "fsck",
        "--connectivity-only",
        "--no-dangling",
        "--no-progress",
    ]);
    let output = process::run(command, b"", OUTPUT_LIMIT, TIMEOUT).await?;
    if output.status.success() {
        return Ok(None);
    }
    let mut report = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    report.push_str(String::from_utf8_lossy(&output.stderr).trim());
    Ok(Some(report))
}

/// Resolve commit IDs in bounded batches, retaining the supplied identity for annotated tags.
pub(crate) async fn existing_commits(path: &Path, ids: &[String]) -> Result<std::collections::HashSet<String>> {
    let mut found = std::collections::HashSet::new();
    for batch in ids.chunks(1024) {
        anyhow::ensure!(batch.iter().all(|id| crate::backend::git::is_full_commit_id(id)), "invalid commit ID");
        let input = batch.iter().map(|id| format!("{id}^{{commit}}\n")).collect::<String>();
        let mut command = git(path)?;
        command.args(["cat-file", "--batch-check"]);
        let output = run(command, input.as_bytes()).await?;
        let output = String::from_utf8(output)?;
        let lines = output.lines().collect::<Vec<_>>();
        anyhow::ensure!(lines.len() == batch.len(), "incomplete commit existence response");
        for (id, line) in batch.iter().zip(lines) {
            if line.split_whitespace().nth(1) == Some("commit") {
                found.insert(id.clone());
            }
        }
    }
    Ok(found)
}

/// Whether the repository holds the commit `sha`.
pub(crate) async fn has_commit(path: &Path, sha: &str) -> Result<bool> {
    let mut command = git(path)?;
    command.args(["cat-file", "-e", &format!("{sha}^{{commit}}")]);
    let output = process::run(command, b"", OUTPUT_LIMIT, TIMEOUT).await?;
    Ok(output.status.success())
}

/// The branch HEAD names when that branch exists, or `None` for a detached or unborn HEAD.
pub(crate) async fn head_branch(path: &Path) -> Result<Option<String>> {
    let mut command = git(path)?;
    command.args(["symbolic-ref", "-q", "HEAD"]);
    let output = process::run(command, b"", OUTPUT_LIMIT, TIMEOUT).await?;
    let target = String::from_utf8_lossy(&output.stdout).strip_suffix('\n').unwrap_or_default().to_owned();
    let Some(branch) = target.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    if !output.status.success() || !has_commit(path, &target).await? {
        return Ok(None);
    }
    Ok(Some(branch.to_owned()))
}

/// The refs under `prefix`, as (name, object id).
pub(crate) async fn refs(path: &Path, prefix: &str) -> Result<Vec<(String, String)>> {
    let mut command = git(path)?;
    command.args(["for-each-ref", "--format=%(refname) %(objectname)", prefix]);
    let output = run(command, b"").await?;
    Ok(String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, sha)| (name.to_owned(), sha.to_owned()))
        .collect())
}

/// Applies `git update-ref --stdin` commands in one transaction.
pub(crate) async fn update_refs(path: &Path, commands: &str) -> Result<()> {
    let mut command = git(path)?;
    command.args(["update-ref", "--stdin"]);
    run(command, commands.as_bytes()).await?;
    Ok(())
}

/// Rebuilds what Git derives from the stored objects: missing pack indexes, then one pack with
/// its commit graph. Nothing is pruned, so unreachable commits a closed pull request
/// or a queued run may still name stay.
pub(crate) async fn rebuild(path: &Path) -> Result<()> {
    for entry in std::fs::read_dir(path.join("objects").join("pack"))? {
        let pack = entry?.path();
        if pack
            .extension()
            .is_some_and(|extension| extension == "pack")
            && !pack.with_extension("idx").exists()
        {
            let mut index = git(path)?;
            index.arg("index-pack").arg(&pack);
            run(index, b"").await?;
        }
    }
    let mut gc = git(path)?;
    // Without autoDetach=false gc would leave the timeout behind in its own session.
    gc.args(["-c", "gc.pruneExpire=never", "-c", "gc.autoDetach=false"])
        .args(["gc", "--quiet"]);
    run(gc, b"").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeSet, ffi::OsString};

    #[tokio::test]
    async fn mirror_copies_commits_that_no_ref_reaches() {
        let root = tempfile::tempdir().unwrap();
        let (source, copy) = (root.path().join("source.git"), root.path().join("copy.git"));
        let repo = git2::Repository::init_bare(&source).unwrap();
        let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        repo.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "kept",
            &tree,
            &[],
        )
        .unwrap();
        // No branch names this commit, but a CI run in the database may still need it.
        let orphan = repo
            .commit(None, &signature, &signature, "orphan", &tree, &[])
            .unwrap();

        mirror(&source, &copy).await.unwrap();

        let copied = git2::Repository::open_bare(&copy).unwrap();
        assert!(copied.find_reference("refs/heads/main").is_ok());
        assert!(copied.find_commit(orphan).is_ok());
    }

    #[tokio::test]
    async fn copying_again_retains_previously_published_packs() {
        let root = tempfile::tempdir().unwrap();
        let (source, copy) = (root.path().join("source.git"), root.path().join("copy.git"));
        let repo = git2::Repository::init_bare(&source).unwrap();
        let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let packs = |path: &Path| -> BTreeSet<OsString> {
            std::fs::read_dir(path.join("objects/pack"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect()
        };
        let first = repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "one",
                &tree,
                &[],
            )
            .unwrap();
        let mut repack = git(&source).unwrap();
        repack.args(["repack", "-a", "-d", "-q"]);
        run(repack, b"").await.unwrap();
        mirror(&source, &copy).await.unwrap();
        // The fetch found every object already copied, so the copy has no pack of its own.
        assert_eq!(packs(&copy), packs(&source));

        let first_packs = packs(&copy);
        let parent = repo.find_commit(first).unwrap();
        repo.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "two",
            &tree,
            &[&parent],
        )
        .unwrap();
        let mut repack = git(&source).unwrap();
        repack.args(["repack", "-a", "-d", "-q"]);
        run(repack, b"").await.unwrap();
        mirror(&source, &copy).await.unwrap();
        assert_eq!(packs(&copy), packs(&source).union(&first_packs).cloned().collect());
        assert!(git2::Repository::open_bare(&copy)
            .unwrap()
            .find_commit(first)
            .is_ok());
    }

    #[test]
    fn deleted_repositories_are_kept_then_removed_after_the_window() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("one.git");
        std::fs::create_dir(&repository).unwrap();
        std::fs::write(repository.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        retire(&repository).unwrap();
        assert!(!repository.exists());
        let kept = deleted_copies(root.path()).unwrap().remove("one.git").unwrap();
        assert!(kept.join("HEAD").is_file());
        assert_eq!(deleted_copies(root.path()).unwrap().remove("two.git"), None);
        // Retiring a repository that is already gone is not an error.
        retire(&repository).unwrap();

        assert_eq!(
            remove_expired_deleted(root.path(), Duration::from_secs(3600)).unwrap(),
            0
        );
        let old = root.path().join(DELETED_DIR).join("1000-two.git");
        std::fs::create_dir(&old).unwrap();
        assert_eq!(
            remove_expired_deleted(root.path(), Duration::from_secs(3600)).unwrap(),
            1
        );
        assert!(!old.exists());
        assert!(kept.exists());
    }
}

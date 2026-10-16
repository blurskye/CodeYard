//! Packing after writes. Git's automatic gc is off for every command this server runs, so
//! nothing else packs a repository. A repository that keeps receiving small pushes would keep
//! its loose objects forever, and large pushes and replica transfers would keep adding packs.
//! After an accepted push the server runs `git gc --auto` in the background when loose
//! objects exceed `gc.auto` or packs exceed `gc.autoPackLimit`. After a replica transfer it
//! checks only packs.
use super::repository::process;
use anyhow::{ensure, Context, Result};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;

/// Git's default `gc.auto`: more loose objects than this and `gc --auto` packs them.
const LOOSE_OBJECT_LIMIT: usize = 6700;
/// Git's default `gc.autoPackLimit`: more packs than this and `gc --auto` repacks them.
const PACK_LIMIT: usize = 50;
/// Repositories packed at the same time.
const RUNS_AT_ONCE: usize = 2;
const RUN_TIMEOUT: Duration = Duration::from_secs(600);
const OUTPUT_LIMIT: usize = 64 * 1024;

/// Repositories with a run queued or in progress.
static QUEUED: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);
static SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(RUNS_AT_ONCE)));

/// Keeps one repository in `QUEUED`. Dropping it lets the next push start a run.
struct RunLock(PathBuf);

impl RunLock {
    /// `None` when a run for `path` is already queued or in progress.
    fn take(path: &Path) -> Option<Self> {
        let mut queued = QUEUED.lock().unwrap_or_else(|poison| poison.into_inner());
        if queued.insert(path.to_path_buf()) {
            Some(Self(path.to_path_buf()))
        } else {
            None
        }
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        QUEUED
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&self.0);
    }
}

/// Packs the repository at `path` in the background after an accepted push. Only the node
/// holding the primary copy gets here. The push does not wait for the run, and a failure is
/// only logged.
pub(crate) fn after_push(repo_id: i64, path: PathBuf) {
    tokio::spawn(async move {
        if let Err(error) = pack_if_needed(&path, LOOSE_OBJECT_LIMIT).await {
            tracing::warn!(repo_id, %error, "repository packing after a push failed");
        }
    });
}

/// Packs a replica copy after a transfer into it, once it has more than `PACK_LIMIT` packs.
/// The copier calls this inside the copy's own job, so no other transfer into the copy runs
/// meanwhile. It never waits for a run slot: a copy that finds them all busy is checked again
/// at its next transfer. Failures are logged.
pub(crate) async fn after_copy(repo_id: i64, mirror: &Path) {
    if let Err(error) = pack_mirror_if_needed(mirror).await {
        tracing::warn!(repo_id, %error, "replica copy packing failed");
    }
}

/// Runs `git gc --auto` when loose objects or packs exceed their limits. A push that
/// finds a run already queued or running for `path` leaves packing to a later push.
async fn pack_if_needed(path: &Path, threshold: usize) -> Result<()> {
    let objects = path.join("objects");
    let check = move || -> Result<bool> {
        Ok(has_more_loose_objects_than(&objects, threshold)?
            || has_more_packs_than(&objects.join("pack"), PACK_LIMIT)?)
    };
    if !tokio::task::spawn_blocking(check).await?? {
        return Ok(());
    }
    let Some(_lock) = RunLock::take(path) else {
        return Ok(());
    };
    let _slot = SLOTS
        .clone()
        .acquire_owned()
        .await
        .context("repository packing closed")?;
    run_gc_auto(path, threshold).await
}

/// Runs `git gc --auto` on a replica copy that has more than `PACK_LIMIT` packs. A copy
/// receives packs rather than loose objects, so only its packs are counted.
async fn pack_mirror_if_needed(mirror: &Path) -> Result<()> {
    let packs = mirror.join("objects").join("pack");
    let check = move || has_more_packs_than(&packs, PACK_LIMIT);
    if !tokio::task::spawn_blocking(check).await?? {
        return Ok(());
    }
    let Some(_lock) = RunLock::take(mirror) else {
        return Ok(());
    };
    let Ok(_slot) = SLOTS.clone().try_acquire_owned() else {
        return Ok(());
    };
    run_gc_auto(mirror, LOOSE_OBJECT_LIMIT).await
}

/// Runs `git gc --auto` with `gc.auto` set to `threshold` and `gc.autoPackLimit` set to
/// `PACK_LIMIT`. Git's own estimate decides whether anything is packed. Nothing is pruned:
/// an unreachable commit can still be a closed pull request's revision, a queued run's
/// commit, or an object of a push in progress, so a run only packs. Space is not reclaimed.
async fn run_gc_auto(path: &Path, threshold: usize) -> Result<()> {
    let mut command = process::git_command()?;
    command.arg("--git-dir").arg(path);
    // Later `-c` options override the scrubbed defaults set in `process::std_command`.
    command.arg("-c").arg(format!("gc.auto={threshold}"));
    let pack_limit = format!("gc.autoPackLimit={PACK_LIMIT}");
    command.arg("-c").arg(pack_limit);
    command.arg("-c").arg("gc.pruneExpire=never");
    // Without this, `gc --auto` detaches into its own session and escapes the timeout.
    command.arg("-c").arg("gc.autoDetach=false");
    command.args(["gc", "--auto", "--quiet"]);
    let output = process::run(command, b"", OUTPUT_LIMIT, RUN_TIMEOUT).await?;
    ensure!(
        output.status.success(),
        "git gc --auto exited with {}",
        output.status
    );
    Ok(())
}

/// Whether more than `limit` loose objects are stored under `objects`. Loose objects sit in
/// the 256 two-digit directories. Counting stops as soon as the answer is known.
fn has_more_loose_objects_than(objects: &Path, limit: usize) -> Result<bool> {
    let mut loose = 0usize;
    for prefix in 0..=255u8 {
        let entries = match std::fs::read_dir(objects.join(format!("{prefix:02x}"))) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            if entry?.file_type()?.is_file() {
                loose += 1;
                if loose > limit {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Whether `pack_dir` holds more than `limit` packs. Only the `.pack` files count; each one
/// has its index beside it.
fn has_more_packs_than(pack_dir: &Path, limit: usize) -> Result<bool> {
    let entries = match std::fs::read_dir(pack_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let mut packs = 0usize;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".pack") && entry.file_type()?.is_file() {
            packs += 1;
            if packs > limit {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loose_objects_are_counted_against_the_limit() {
        let root = tempfile::tempdir().unwrap();
        let objects = root.path().join("objects");
        for (directory, name) in [("ab", "cdef"), ("ab", "0123"), ("ff", "beef")] {
            let directory = objects.join(directory);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join(name), b"").unwrap();
        }
        // Packs are not loose objects.
        std::fs::create_dir_all(objects.join("pack")).unwrap();
        std::fs::write(objects.join("pack").join("pack-1.pack"), b"").unwrap();
        assert!(has_more_loose_objects_than(&objects, 2).unwrap());
        assert!(!has_more_loose_objects_than(&objects, 3).unwrap());
    }

    #[test]
    fn a_copy_is_packed_only_above_the_pack_limit() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("pack");
        std::fs::create_dir_all(&packs).unwrap();
        for index in 0..PACK_LIMIT {
            std::fs::write(packs.join(format!("pack-{index}.pack")), b"").unwrap();
            std::fs::write(packs.join(format!("pack-{index}.idx")), b"").unwrap();
        }
        assert!(!has_more_packs_than(&packs, PACK_LIMIT).unwrap());
        std::fs::write(packs.join("pack-extra.pack"), b"").unwrap();
        assert!(has_more_packs_than(&packs, PACK_LIMIT).unwrap());
        assert!(!has_more_packs_than(&root.path().join("missing"), PACK_LIMIT).unwrap());
    }

    #[test]
    fn one_run_per_repository_at_a_time() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.git");
        let second = root.path().join("second.git");
        let lock = RunLock::take(&first).unwrap();
        assert!(RunLock::take(&first).is_none());
        assert!(RunLock::take(&second).is_some());
        drop(lock);
        assert!(RunLock::take(&first).is_some());
    }

    /// A commit that no ref reaches, such as the revision of a closed pull request or the
    /// commit of a queued run, is still readable after a month, when a run has packed.
    #[tokio::test]
    async fn gc_auto_never_prunes_unreachable_objects() {
        fn blobs_starting_17(count: usize, from: usize) -> Vec<String> {
            (from..)
                .map(|index| format!("content {index}"))
                .filter(|content| {
                    git2::Oid::hash_object(git2::ObjectType::Blob, content.as_bytes())
                        .unwrap()
                        .to_string()
                        .starts_with("17")
                })
                .take(count)
                .collect()
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("repo.git");
        let repo = git2::Repository::init_bare(&path).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let old = blobs_starting_17(2, 0);
        let mut builder = repo.treebuilder(None).unwrap();
        for (index, content) in old.iter().enumerate() {
            let blob = repo.blob(content.as_bytes()).unwrap();
            builder
                .insert(format!("file{index}"), blob, 0o100644)
                .unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        // No ref points at this commit.
        let commit = repo
            .commit(None, &signature, &signature, "old", &tree, &[])
            .unwrap();
        let month_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86_400);
        let mut stack = vec![path.join("objects")];
        while let Some(directory) = stack.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                } else {
                    std::fs::File::open(entry.path())
                        .unwrap()
                        .set_modified(month_ago)
                        .unwrap();
                }
            }
        }
        // New loose objects, so that the run has something to pack.
        let fresh = repo
            .blob(blobs_starting_17(3, 10_000).last().unwrap().as_bytes())
            .unwrap();
        repo.reference(
            "refs/heads/main",
            {
                let mut builder = repo.treebuilder(None).unwrap();
                builder.insert("fresh", fresh, 0o100644).unwrap();
                let tree = repo.find_tree(builder.write().unwrap()).unwrap();
                repo.commit(None, &signature, &signature, "fresh", &tree, &[])
                    .unwrap()
            },
            true,
            "main",
        )
        .unwrap();
        assert!(has_more_loose_objects_than(&path.join("objects"), 1).unwrap());

        pack_if_needed(&path, 1).await.unwrap();

        let repo = git2::Repository::open_bare(&path).unwrap();
        let tree = repo.find_commit(commit).unwrap().tree().unwrap();
        for index in 0..old.len() {
            let entry = tree.get_name(&format!("file{index}")).unwrap();
            assert!(repo.find_blob(entry.id()).is_ok());
        }
    }

    /// Runs the real `git gc --auto` with a threshold of 1. Git packs once `objects/17`
    /// holds more than one loose object, so the blobs are picked to have IDs starting 17.
    #[tokio::test]
    async fn gc_auto_packs_loose_objects_and_the_repository_still_reads() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("repo.git");
        let repo = git2::Repository::init_bare(&path).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let mut contents = Vec::new();
        let mut index = 0;
        while contents.len() < 2 {
            let content = format!("content {index}");
            index += 1;
            let id = git2::Oid::hash_object(git2::ObjectType::Blob, content.as_bytes());
            if id.unwrap().to_string().starts_with("17") {
                contents.push(content);
            }
        }
        let mut builder = repo.treebuilder(None).unwrap();
        for (index, content) in contents.iter().enumerate() {
            let blob = repo.blob(content.as_bytes()).unwrap();
            let name = format!("file{index}.txt");
            builder.insert(name, blob, 0o100644).unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let commit = repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "two files",
                &tree,
                &[],
            )
            .unwrap();
        assert!(has_more_loose_objects_than(&path.join("objects"), 1).unwrap());

        pack_if_needed(&path, 1).await.unwrap();

        assert!(!has_more_loose_objects_than(&path.join("objects"), 0).unwrap());
        let mut names = std::fs::read_dir(path.join("objects/pack"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name());
        assert!(names.any(|name| name.to_string_lossy().ends_with(".pack")));
        let repo = git2::Repository::open_bare(&path).unwrap();
        let tree = repo.find_commit(commit).unwrap().tree().unwrap();
        for (index, content) in contents.iter().enumerate() {
            let entry = tree.get_name(&format!("file{index}.txt")).unwrap();
            let blob = repo.find_blob(entry.id()).unwrap();
            assert_eq!(blob.content(), content.as_bytes());
        }
    }
}

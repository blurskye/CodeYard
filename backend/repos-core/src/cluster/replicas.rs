use std::path::{Path,PathBuf};
use std::time::Duration;
use uuid::Uuid;
use anyhow::{Context,Result};
use codeyard_git::{locks,mirror};
use tokio::sync::{OwnedMutexGuard,OwnedRwLockWriteGuard};
/// How long moving or retiring a directory waits for the Git sessions using it. A directory
/// that stays busy is left for a later pass, so a long clone holds nothing else up.
pub const MOVE_WAIT: Duration = Duration::from_millis(500);
/// Copies are kept here, under the repositories root, never at a primary's location.
pub const MIRRORS: &str = "mirrors";
pub const QUARANTINES: &str = ".quarantines";

pub fn copy_path(repos: &Path, uuid: Uuid) -> PathBuf {
    repos.join(MIRRORS).join(format!("{uuid}.git"))
}

/// Whether Git can open the copy at `mirror`. Reconcile and the copy path share this check, so
/// a copy counts as usable only when Git can open it.
async fn opens(mirror: &Path) -> Result<bool> {
    let mirror = mirror.to_owned();
    tokio::task::spawn_blocking(move || codeyard_git::repository::opens_as_repository(&mirror))
        .await
        .context("the copy check stopped")?
}

pub async fn connected(path: &Path) -> Result<bool> {
    Ok(opens(path).await? && mirror::check(path).await?.is_none())
}

/// Keeps writers and readers out of a repository's primary and copy directories while one
/// of them is replaced or moved.
pub struct Moving {
    _writers: (OwnedMutexGuard<()>, OwnedMutexGuard<()>),
    _readers: (OwnedRwLockWriteGuard<()>, OwnedRwLockWriteGuard<()>),
}

/// Takes both writer locks, then waits for the readers of both directories to finish. Every
/// mover takes them in this order.
pub async fn moving(primary: &Path, mirror: &Path) -> Moving {
    let writers = (locks::lock(primary).await, locks::lock(mirror).await);
    let readers = (
        locks::exclusive(primary).await,
        locks::exclusive(mirror).await,
    );
    Moving {
        _writers: writers,
        _readers: readers,
    }
}


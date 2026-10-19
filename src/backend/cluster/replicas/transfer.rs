//! Bringing one copy up to its primary. The primary lists its HEAD and refs, and streams
//! a pack of the objects they need beyond the copy's tips. The copy stores the pack, then
//! sets its refs and HEAD to the listing. Git does the object and ref work. A bundle is
//! not used: it leaves out the refs that point into the copy's history, so a copy that is
//! ahead of a tag would lose the tag.
use crate::backend::{
    cluster::{
        rpc::{Operation, Request},
        NodeContext,
    },
    repositories::store::{GitStreams, RepoStore},
    storage::local::TEMP_PREFIX,
};
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use std::{collections::HashMap, path::Path};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

/// What the primary sends before its pack.
#[derive(Deserialize)]
struct Listing {
    /// The branch the primary's HEAD names, if it names one.
    head: Option<String>,
    /// Every ref the primary holds, with the commit each one points at.
    refs: HashMap<String, String>,
}

/// Brings the copy at `mirror` to the refs the primary holds for `repository`. A copy
/// that does not exist yet is created.
pub(super) async fn copy(
    context: &NodeContext,
    primary: i64,
    repository: Uuid,
    mirror: &Path,
) -> Result<()> {
    // A copy Git cannot open has no refs to offer, so the primary sends everything.
    let have = if super::opens(mirror).await? {
        tips(mirror).await?
    } else {
        Vec::new()
    };
    match fetch(context, primary, repository, mirror, &have).await {
        // A tip the primary no longer has makes the incremental pack fail. Then the
        // primary sends everything the copy needs. A busy primary is asked later instead.
        Err(error) if !have.is_empty() && !super::busy(&error) => {
            tracing::debug!(%error, "incremental replica copy failed; copying everything");
            fetch(context, primary, repository, mirror, &[]).await
        }
        result => result,
    }
}

/// The distinct commits at the copy's refs. The primary leaves their history out of the pack.
async fn tips(mirror: &Path) -> Result<Vec<String>> {
    let path = mirror.to_str().context("replica copy path is not text")?;
    let mut tips: Vec<String> = RepoStore::from_disk_path(path)
        .mirror_refs()
        .await?
        .into_values()
        .collect();
    tips.sort();
    tips.dedup();
    Ok(tips)
}

async fn fetch(
    context: &NodeContext,
    primary: i64,
    repository: Uuid,
    mirror: &Path,
    have: &[String],
) -> Result<()> {
    let copies = mirror.parent().context("replica copy has no directory")?;
    tokio::fs::create_dir_all(copies).await?;
    // The pack is written beside the copies, on the same disk, and removed on return. A
    // crash skips that, so the prefix lets `remove_stale_packs` find what was left.
    let pack = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(copies)?;
    let mut output = tokio::fs::File::from_std(pack.reopen()?);
    let (listing, status) = context
        .git_session(
            primary,
            Request {
                repository,
                operation: Operation::ReplicaPack {
                    have: have.to_vec(),
                },
                actor: None,
                ci_run: None,
            },
            GitStreams {
                input: &mut tokio::io::empty(),
                output: &mut output,
                errors: &mut tokio::io::sink(),
            },
        )
        .await?;
    ensure!(status.success(), "the primary could not make a pack");
    output.flush().await?;
    drop(output);
    let listing: Listing = serde_json::from_value(listing)?;
    let store =
        RepoStore::from_disk_path(mirror.to_str().context("replica copy path is not text")?);
    // `initialize` needs an empty path, so a directory Git cannot open is renamed aside first.
    if !super::opens(mirror).await? {
        if mirror.exists() {
            move_aside(mirror).await?;
        }
        store.initialize(None, "main").await?;
    }
    store.index_pack(pack.path()).await?;
    store.set_refs(&listing.refs).await?;
    if let Some(head) = listing.head {
        store.set_head_target(&head).await?;
    }
    // The copy is complete, so the damaged copies it replaced can go. A fetch that fails
    // returns above and keeps them. This runs after every successful fetch: a rebuild that was
    // interrupted after its refs were set leaves a usable copy, and the retry is incremental.
    remove_aside_copies(mirror).await;
    Ok(())
}

/// Renames a copy that Git cannot open to the sibling `<name>.unusable-<millis>`. The name does
/// is removed once a complete replacement exists, by `remove_aside_copies`, or when
/// surplus cleanup finds this node no longer holds the repository.
async fn move_aside(mirror: &Path) -> Result<()> {
    let name = mirror
        .file_name()
        .context("replica copy has no name")?
        .to_string_lossy();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let aside = mirror.with_file_name(format!("{name}.unusable-{stamp}"));
    tokio::fs::rename(mirror, &aside).await?;
    Ok(())
}

/// Removes every sibling named `<name>.unusable-<digits>` of `mirror`. Other names, files and
/// symlinks are left alone. A directory that cannot be removed is logged and left for the
/// next copy.
async fn remove_aside_copies(mirror: &Path) {
    let (Some(directory), Some(name)) = (mirror.parent(), mirror.file_name()) else {
        return;
    };
    let prefix = format!("{}.unusable-", name.to_string_lossy());
    let mut entries = match tokio::fs::read_dir(directory).await {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(path = %directory.display(), %error, "could not list replica copies");
            return;
        }
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let file_name = entry.file_name();
        let is_aside = file_name
            .to_string_lossy()
            .strip_prefix(&prefix)
            .is_some_and(|stamp| !stamp.is_empty() && stamp.bytes().all(|b| b.is_ascii_digit()));
        // `DirEntry::file_type` does not follow symlinks.
        let is_directory = entry.file_type().await.is_ok_and(|kind| kind.is_dir());
        if is_aside && is_directory {
            if let Err(error) = tokio::fs::remove_dir_all(entry.path()).await {
                tracing::warn!(path = %entry.path().display(), %error, "could not remove a quarantined replica copy");
            }
        }
    }
}

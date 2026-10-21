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

#[derive(Debug, thiserror::Error)]
#[error("the primary could not make an incremental pack")]
struct IncrementalPackRejected;

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
        Err(error) if error.is::<IncrementalPackRejected>() => {
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
    if !status.success() && !have.is_empty() {
        return Err(IncrementalPackRejected.into());
    }
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

/// Renames a copy that Git cannot open to the sibling `<name>.unusable-<millis>`. The renamed
/// copy is removed once a complete replacement exists, by `remove_aside_copies`, or when
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::cluster::{rpc, CorePeer, IrohDialer, NodeAddress, NodeKey, Relay};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    async fn failed_copy(primary_rejects: bool) -> usize {
        let root = tempfile::tempdir().unwrap();
        let mirror = root.path().join("mirror.git");
        {
            let repo = git2::Repository::init_bare(&mirror).unwrap();
            let tree = repo
                .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
                .unwrap();
            let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
            repo.commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "tip",
                &tree,
                &[],
            )
            .unwrap();
        }
        let key = NodeKey::generate();
        let server_key = NodeKey::generate();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let context = NodeContext {
            pool: None,
            iroh: IrohDialer::new(Relay::Off, key.clone()),
            key,
            node_id: 1,
            repos_path: root.path().to_owned(),
            peers: Default::default(),
            unreachable: Default::default(),
            ci_replicas: Default::default(),
        };
        context.remember_peer(
            2,
            CorePeer {
                public_key: server_key.public_hex(),
                address: NodeAddress {
                    tcp: vec![listener.local_addr().unwrap()],
                    ..Default::default()
                },
            },
        );
        let acceptor = tokio_rustls::TlsAcceptor::from(
            crate::backend::cluster::tls::server_config(&server_key).unwrap(),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let server = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(socket).await.unwrap();
                let header = rpc::read_frame(&mut stream, rpc::MAX_JSON).await.unwrap();
                let request: Request = serde_json::from_slice(&header).unwrap();
                let call = count.fetch_add(1, Ordering::SeqCst);
                let Operation::ReplicaPack { have } = request.operation else {
                    panic!("not a pack");
                };
                assert_eq!(have.is_empty(), call > 0);
                let reply = rpc::Reply {
                    result: Ok(serde_json::json!({"head":null,"refs":{}})),
                };
                rpc::write_frame(
                    &mut stream,
                    &serde_json::to_vec(&reply).unwrap(),
                    rpc::MAX_JSON,
                )
                .await
                .unwrap();
                // An invalid pack models a local indexing failure after a successful download.
                rpc::write_frame(&mut stream, b"invalid pack", 32768)
                    .await
                    .unwrap();
                rpc::write_frame(&mut stream, b"", 32768).await.unwrap();
                let code = i32::from(primary_rejects && call == 0);
                let exit =
                    serde_json::to_vec(&serde_json::json!({"result":{"Ok":code},"stderr":""}))
                        .unwrap();
                rpc::write_frame(&mut stream, &exit, rpc::MAX_JSON)
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        assert!(copy(&context, 2, Uuid::new_v4(), &mirror).await.is_err());
        let calls = calls.load(Ordering::SeqCst);
        server.abort();
        let _ = server.await;
        calls
    }

    #[tokio::test]
    async fn indexing_failure_does_not_download_the_whole_repository_again() {
        assert_eq!(failed_copy(false).await, 1);
    }

    #[tokio::test]
    async fn rejected_incremental_pack_is_retried_without_have_tips() {
        assert_eq!(failed_copy(true).await, 2);
    }
}

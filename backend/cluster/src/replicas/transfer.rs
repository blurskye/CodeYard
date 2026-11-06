//! Bringing one copy up to its primary. The primary lists its HEAD and refs, and streams
//! a pack of the objects they need beyond the copy's tips. The copy stores the pack, then
//! sets its refs and HEAD to the listing. Git does the object and ref work. A bundle is
//! not used: it leaves out the refs that point into the copy's history, so a copy that is
//! ahead of a tag would lose the tag.
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use codeyard_repos_core::context::NodeContext;
use codeyard_repos_core::repositories::store::RepoStore;
use codeyard_repos_core::store_types::GitStreams;
use codeyard_repos_core::wire::Operation;
use codeyard_repos_core::wire::Request;
use codeyard_storage::local::TEMP_PREFIX;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
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
    stop: &CancellationToken,
) -> Result<()> {
    // No directory mutation is admitted during preparation or download.
    let have = tokio::select! {
        biased;
        _ = stop.cancelled() => anyhow::bail!("replica assignment changed"),
        have = async {
            if codeyard_repos_core::cluster::replicas::connected(mirror).await? {
                tips(context, repository, mirror).await
            } else {
                Ok(Vec::new())
            }
        } => have?,
    };
    // The primary filters missing have tips before packing. Other failures must not trigger
    // another, more expensive full pack with the same underlying failure.
    fetch(context, primary, repository, mirror, &have, stop).await
}

/// The distinct commits at the copy's refs. The primary leaves their history out of the pack.
async fn tips(context: &NodeContext, repository: Uuid, mirror: &Path) -> Result<Vec<String>> {
    let path = mirror.to_str().context("replica copy path is not text")?;
    let store = RepoStore::from_disk_path(path);
    let mut tips: Vec<String> = store.mirror_refs().await?.into_values().collect();
    let pool = context.database()?;
    let row = codeyard_db_cluster::cluster::get_repository_by_uuid(pool, &repository.to_string())
        .await?
        .context("replica repository was deleted")?;
    let mut retained = codeyard_db_actions::ci::retained_ci_commits(pool, row.id).await?;
    retained
        .extend(codeyard_db_repos::pull_requests::records::retained_revisions(pool, row.id).await?);
    tips.extend(store.existing_commits(&retained).await?);
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
    stop: &CancellationToken,
) -> Result<()> {
    let (pack, listing) = download(context, primary, repository, mirror, have, &[], stop).await?;
    let store =
        RepoStore::from_disk_path(mirror.to_str().context("replica copy path is not text")?);
    let pool = context.database()?;
    let row = codeyard_db_cluster::cluster::get_repository_by_uuid(pool, &repository.to_string())
        .await?
        .context("replica repository was deleted")?;
    let primary_path =
        codeyard_repos_core::repositories::store::primary_path(&context.repos_path, repository);
    let _installation = tokio::select! {
        biased;
        _ = stop.cancelled() => anyhow::bail!("replica assignment changed"),
        guards = codeyard_repos_core::cluster::replicas::moving(&primary_path, mirror) => guards,
    };
    // From here cancellation is deferred until installation has ended. Copying retains
    // the task and its directory reservation until join, including during shutdown.
    ensure!(
        codeyard_db_cluster::cluster::set_applied_version(
            pool,
            row.id,
            context.node_id,
            primary,
            -1
        )
        .await?,
        "replica assignment changed before installation"
    );
    // `initialize` needs an empty path, so a directory Git cannot open is renamed aside first.
    if !codeyard_repos_core::cluster::replicas::connected(mirror).await? {
        if mirror.exists() {
            move_aside(mirror).await?;
        }
        store.initialize(None, "main").await?;
    }
    store.index_pack(pack.path(), false).await?;
    if let Err(error) = store.set_refs(&listing.refs).await {
        // An interrupted update-ref may leave a lock that never recovers through retries.
        // This mirror is exclusively owned and already marked incomplete; rebuild it next time.
        move_aside(mirror).await?;
        return Err(error.into());
    }
    if let Some(head) = listing.head {
        store.set_head_target(&head).await?;
    }
    // The copy is complete, so the damaged copies it replaced can go. A fetch that fails
    // returns above and keeps them. This runs after every successful fetch: a rebuild that was
    // interrupted after its refs were set leaves a usable copy, and the retry is incremental.
    remove_aside_copies(mirror).await;
    Ok(())
}

/// Quarantines an unusable mirror under its own directory until a replacement is complete.
async fn move_aside(mirror: &Path) -> Result<()> {
    let name = mirror
        .file_name()
        .context("replica copy has no name")?
        .to_string_lossy();
    let directory = mirror
        .parent()
        .context("replica copy has no parent")?
        .join(codeyard_repos_core::cluster::replicas::QUARANTINES)
        .join(name.as_ref());
    tokio::fs::create_dir_all(&directory).await?;
    let aside = directory.join(Uuid::new_v4().to_string());
    tokio::fs::rename(mirror, &aside).await?;
    Ok(())
}

/// Cleans only this repository's quarantines; unrelated mirrors are never enumerated.
async fn remove_aside_copies(mirror: &Path) {
    let (Some(directory), Some(name)) = (mirror.parent(), mirror.file_name()) else {
        return;
    };
    let directory = directory
        .join(codeyard_repos_core::cluster::replicas::QUARANTINES)
        .join(name);
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(path = %directory.display(), %error, "could not list replica copies");
            return;
        }
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let is_aside = entry
            .file_name()
            .to_str()
            .is_some_and(|name| Uuid::parse_str(name).is_ok());
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

    use codeyard_cluster_core::transport::IrohDialer;
    use codeyard_cluster_core::transport::NodeAddress;
    use codeyard_cluster_core::transport::Relay;
    use codeyard_support::node_identity::NodeKey;

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
        // The copy reads its repository row and its retained commits, and marks the copy
        // incomplete before it installs the pack.
        let pool = codeyard_db_core::init_db(&format!(
            "sqlite://{}",
            root.path().join("test.db").display()
        ))
        .await
        .unwrap();
        codeyard_db_core::run_migrations(&pool).await.unwrap();
        let mut nodes = Vec::new();
        for (node_key, name) in [
            (key.public_hex(), "copy"),
            (server_key.public_hex(), "primary"),
        ] {
            let roles = ["storage".to_owned()];
            let node = codeyard_db_cluster::cluster::register_own_node(
                &pool, &node_key, name, &roles, "test",
            )
            .await;
            nodes.push(node.unwrap().id);
        }
        let (copy_node, primary) = (nodes[0], nodes[1]);
        let owner = codeyard_db_identity::users::accounts::create_user(&pool, "owner", None)
            .await
            .unwrap();
        let repository = Uuid::new_v4();
        let row = codeyard_db_repos::repositories::records::create_repository(
            &pool,
            owner.id,
            "copied",
            None,
            false,
            &format!("/repos/{repository}.git"),
        )
        .await
        .unwrap();
        assert!(
            codeyard_db_cluster::cluster::place_repository(&pool, row.id, primary)
                .await
                .unwrap()
        );
        assert!(
            codeyard_db_cluster::cluster::insert_replica(&pool, row.id, copy_node, primary)
                .await
                .unwrap()
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let context = NodeContext {
            pool: Some(pool),
            rpc: std::sync::Arc::new(codeyard_repos_rpc::RpcFactory),
            iroh: IrohDialer::new(Relay::Off, key.clone()),
            key,
            node_id: copy_node,
            repos_path: root.path().to_owned(),
            peers: Default::default(),
            unreachable: Default::default(),
            clone_cursor: Default::default(),
            ci_replicas: Default::default(),
        };
        // With a database, the copy dials the address in the primary's node row.
        let address = NodeAddress {
            tcp: vec![listener.local_addr().unwrap()],
            ..Default::default()
        };
        let hint = serde_json::to_string(&address).unwrap();
        let pool = context.database().unwrap();
        assert!(codeyard_db_cluster::cluster::record_node_heartbeat(
            pool,
            &server_key.public_hex(),
            "test",
            &hint,
            Some(1 << 40)
        )
        .await
        .unwrap());
        let acceptor = tokio_rustls::TlsAcceptor::from(
            codeyard_cluster_core::tls::server_config(&server_key).unwrap(),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let server = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(socket).await.unwrap();
                let header = codeyard_cluster_core::link::read_frame(
                    &mut stream,
                    codeyard_db_core::limits::MAX_JSON,
                )
                .await
                .unwrap();
                let request: Request = serde_json::from_slice(&header).unwrap();
                let call = count.fetch_add(1, Ordering::SeqCst);
                let Operation::ReplicaPack { have, .. } = request.operation else {
                    panic!("not a pack");
                };
                assert_eq!(have.is_empty(), call > 0);
                let reply = codeyard_repos_core::wire::Reply {
                    result: Ok(serde_json::json!({"head":null,"refs":{}})),
                };
                codeyard_cluster_core::link::write_frame(
                    &mut stream,
                    &serde_json::to_vec(&reply).unwrap(),
                    codeyard_db_core::limits::MAX_JSON,
                )
                .await
                .unwrap();
                // An invalid pack models a local indexing failure after a successful download.
                codeyard_cluster_core::link::write_frame(&mut stream, b"invalid pack", 32768)
                    .await
                    .unwrap();
                codeyard_cluster_core::link::write_frame(&mut stream, b"", 32768)
                    .await
                    .unwrap();
                let code = i32::from(primary_rejects && call == 0);
                let exit =
                    serde_json::to_vec(&serde_json::json!({"result":{"Ok":code},"stderr":""}))
                        .unwrap();
                codeyard_cluster_core::link::write_frame(
                    &mut stream,
                    &exit,
                    codeyard_db_core::limits::MAX_JSON,
                )
                .await
                .unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        assert!(copy(
            &context,
            primary,
            repository,
            &mirror,
            &CancellationToken::new()
        )
        .await
        .is_err());
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
    async fn pack_failure_does_not_trigger_an_unrelated_full_retry() {
        assert_eq!(failed_copy(true).await, 1);
    }
}

/// All replica streams in this process share one bandwidth budget, including fork families.
struct Throttled<'a> {
    inner: &'a mut tokio::fs::File,
    rate: u64,
    wait: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    reserved: usize,
}

impl tokio::io::AsyncWrite for Throttled<'_> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::future::Future;
        use std::task::Poll;
        static NEXT: std::sync::LazyLock<std::sync::Mutex<tokio::time::Instant>> =
            std::sync::LazyLock::new(|| std::sync::Mutex::new(tokio::time::Instant::now()));
        let this = self.get_mut();
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if this.wait.is_none() {
            let mut next = NEXT.lock().unwrap_or_else(|error| error.into_inner());
            let ready = (*next).max(tokio::time::Instant::now());
            this.reserved = bytes.len().min(32 * 1024);
            let nanos = (this.reserved as u64)
                .saturating_mul(1_000_000_000)
                .div_ceil(this.rate);
            *next = ready + std::time::Duration::from_nanos(nanos);
            this.wait = Some(Box::pin(tokio::time::sleep_until(ready)));
        }
        if this
            .wait
            .as_mut()
            .expect("reserved transfer budget")
            .as_mut()
            .poll(cx)
            .is_pending()
        {
            return Poll::Pending;
        }
        let result = std::pin::Pin::new(&mut *this.inner)
            .poll_write(cx, &bytes[..bytes.len().min(this.reserved)]);
        if result.is_ready() {
            this.wait = None;
        }
        result
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod bandwidth_tests {
    use super::*;
    #[tokio::test]
    async fn replica_output_obeys_its_bandwidth_budget() {
        let file = tempfile::tempfile().unwrap();
        let mut file = tokio::fs::File::from_std(file);
        let mut output = Throttled {
            inner: &mut file,
            rate: 32 * 1024,
            wait: None,
            reserved: 0,
        };
        let started = std::time::Instant::now();
        output.write_all(&[0u8; 64 * 1024]).await.unwrap();
        output.flush().await.unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(900));
    }
}

async fn download(
    context: &NodeContext,
    primary: i64,
    repository: Uuid,
    mirror: &Path,
    have: &[String],
    want: &[String],
    stop: &CancellationToken,
) -> Result<(tempfile::NamedTempFile, Listing)> {
    let copies = mirror.parent().context("replica copy has no directory")?;
    tokio::fs::create_dir_all(copies).await?;
    // The pack is written beside the copies, on the same disk, and removed on return. A
    // crash skips that, so the prefix lets `remove_stale_packs` find what was left.
    let pack = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(copies)?;
    let mut output = tokio::fs::File::from_std(pack.reopen()?);
    let rate: u64 = std::env::var("REPLICA_BANDWIDTH_BYTES_PER_SECOND")
        .map(|value| value.parse())
        .unwrap_or(Ok(50 * 1024 * 1024))?;
    ensure!(
        (1024..=1024 * 1024 * 1024).contains(&rate),
        "invalid replica bandwidth limit"
    );
    let mut limited = Throttled {
        inner: &mut output,
        rate,
        wait: None,
        reserved: 0,
    };
    let max_bytes = match std::env::var("REPLICA_MAX_PACK_BYTES") {
        Ok(raw) => raw.parse::<u64>().context("invalid replica pack limit")?,
        Err(std::env::VarError::NotPresent) => 4 * 1024 * 1024 * 1024,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        (1..=1024 * 1024 * 1024 * 1024).contains(&max_bytes),
        "invalid replica pack limit"
    );
    let mut bounded =
        codeyard_repos_core::git_transport::LimitedWriter::new(&mut limited, max_bytes);
    let mut input = tokio::io::empty();
    let mut errors = tokio::io::sink();
    let (listing, status) = tokio::select! {
        biased;
        _ = stop.cancelled() => anyhow::bail!("replica assignment changed"),
        downloaded = context.git_session(
            primary,
            Request {
                repository,
                operation: Operation::ReplicaPack {
                    have: have.to_vec(),
                    want: want.to_vec(),
                },
                actor: None,
                ci_run: None,
            },
            GitStreams {
                input: &mut input,
                output: &mut bounded,
                errors: &mut errors,
            },
        ) => downloaded?,
    };
    ensure!(status.success(), "the primary could not make a pack");
    drop(limited);
    output.flush().await?;
    drop(output);
    let listing: Listing = serde_json::from_value(listing)?;
    Ok((pack, listing))
}

/// Mirrors a primary for operator backup without creating or changing a replica assignment.
pub(super) async fn snapshot(
    context: &NodeContext,
    primary: i64,
    repository: Uuid,
    mirror: &Path,
    want: &[String],
    stop: &CancellationToken,
) -> Result<()> {
    let have = if mirror.join("HEAD").is_file() {
        tips(context, repository, mirror).await?
    } else {
        Vec::new()
    };
    let (pack, listing) = download(context, primary, repository, mirror, &have, want, stop).await?;
    let store = RepoStore::from_disk_path(mirror.to_str().context("backup path is not text")?);
    if !mirror.join("HEAD").is_file() {
        store.initialize(None, "main").await?;
    }
    store.index_pack(pack.path(), false).await?;
    let present = store.existing_commits(want).await?;
    ensure!(
        want.iter().all(|id| present.contains(id)),
        "primary omitted a revision named by the database snapshot"
    );
    store.set_refs(&listing.refs).await?;
    if let Some(head) = listing.head {
        store.set_head_target(&head).await?;
    }
    Ok(())
}

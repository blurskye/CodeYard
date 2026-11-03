use super::{
    client::{attempt_deadline, cannot_serve, log_failover},
    wire::*,
    RpcHandler,
};
use crate::backend::db::{self, cluster::writes as journal, models::WriteRecord};
use crate::backend::{
    ci::{CiConfig, CiRuns},
    cluster::{link::Link, transport, NodeContext},
    git::repository::process as git_process,
    repositories::store::{GitService, GitStreams, RepoError, RepoStore},
};
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::sync::Arc;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;

/// A session the node accepted: the link it runs on and its first reply.
type Opened = (Link, Reply);

impl NodeContext {
    /// Runs a streamed operation on `node_id`. Returns the value the node put in its
    /// reply and the exit status of the stream. A read-only operation whose primary gave no
    /// reply is tried on its copies; that happens before any input or output has moved.
    pub async fn git_session(
        &self,
        node_id: i64,
        request: Request,
        streams: GitStreams<'_>,
    ) -> anyhow::Result<(Value, std::process::ExitStatus)> {
        use std::os::unix::process::ExitStatusExt;
        let receive = matches!(&request.operation, Operation::GitSession(session) if session.service == GitService::ReceivePack);
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        let mut opened = self
            .open_session(
                node_id,
                &request,
                if request.operation.is_read_only() {
                    attempt_deadline(deadline)
                } else {
                    deadline
                },
            )
            .await;
        if request.operation.is_read_only()
            && cannot_serve(opened.as_ref().ok().map(|(_, reply)| reply), true)
        {
            let copies =
                tokio::time::timeout_at(deadline, self.read_copies(request.repository, node_id))
                    .await
                    .map_err(|_| unavailable())?;
            let mut copies = copies.into_iter().peekable();
            while let Some((copy, moved)) = copies.next() {
                let candidate_deadline = if copies.peek().is_none() {
                    deadline
                } else {
                    attempt_deadline(deadline)
                };
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                if copy == self.node_id {
                    // The permit comes before admission, so a key revoked while this waited
                    // is refused. Admission comes before the streams are touched, so a copy
                    // this node cannot serve leaves them for the next one.
                    let Ok(permit) =
                        tokio::time::timeout_at(candidate_deadline, git_process::session_permit())
                            .await
                    else {
                        continue;
                    };
                    let Ok(_permit) = permit else {
                        continue;
                    };
                    if let Ok(Ok((store, checkout, _read_guard))) =
                        tokio::time::timeout_at(candidate_deadline, self.own_copy(&request)).await
                    {
                        let Operation::GitSession(mut session) = request.operation else {
                            return Err(unavailable().into());
                        };
                        session.allow_unreachable = checkout;
                        // Boxed: the store's remote variant leads back to this function.
                        let status = Box::pin(store.git_session(session, None, streams)).await?;
                        return Ok((Value::Null, status));
                    }
                    continue;
                }
                let answer = self.open_session(copy, &request, candidate_deadline).await;
                if !cannot_serve(answer.as_ref().ok().map(|(_, reply)| reply), moved) {
                    log_failover(request.repository, node_id, copy);
                    opened = answer;
                    break;
                }
            }
        }
        let (mut link, reply) = opened?;
        let value = reply.result.map_err(|failure| match failure {
            Failure::Repository(error) => error,
            Failure::Merge(_) => unavailable(),
        })?;
        let (activity, _receiver) = tokio::sync::watch::channel(tokio::time::Instant::now());
        let writer = &mut link.writer;
        let reader = &mut link.reader;
        let work = async {
            let inbound = async {
                let result =
                    git_process::copy(streams.input, &mut *writer, activity.clone(), false).await;
                let _ = writer.shutdown().await;
                result
            };
            let outbound = async {
                let mut disconnected = false;
                loop {
                    let bytes = read_frame(&mut *reader, git_process::CHUNK).await?;
                    if bytes.is_empty() {
                        break;
                    }
                    activity.send_replace(tokio::time::Instant::now());
                    if !disconnected {
                        match streams.output.write_all(&bytes).await {
                            Ok(()) => {
                                activity.send_replace(tokio::time::Instant::now());
                            }
                            Err(_) if receive => disconnected = true,
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
                let reply: GitExit =
                    serde_json::from_slice(&read_frame(&mut *reader, MAX_JSON).await?)?;
                let _ = streams.errors.write_all(reply.stderr.as_bytes()).await;
                let code = reply.result?;
                Ok(std::process::ExitStatus::from_raw(code.clamp(0, 255) << 8))
            };
            tokio::pin!(inbound, outbound);
            tokio::select! {
                result = &mut outbound => result,
                result = &mut inbound => {
                    if let Err(error) = result {
                        if !receive { return Err(error); }
                    }
                    outbound.await
                }
            }
        };
        // Only storage observes Git stderr progress. Its idle timer is authoritative;
        // forwarding retains a total deadline without mistaking stderr-only work for idle.
        let result = tokio::time::timeout(git_process::SESSION_TIMEOUT, work)
            .await
            .map_err(|_| anyhow::anyhow!("Git session timed out"))
            .and_then(|result| result);
        link.close(0, b"Git session complete");
        result.map(|status| (value, status))
    }

    /// The store of the copy this node holds. A node cannot dial itself, so the handler's
    /// own checks run in process. The caller already holds a session permit.
    async fn own_copy(
        &self,
        request: &Request,
    ) -> Result<(RepoStore, bool, tokio::sync::OwnedRwLockReadGuard<()>), RepoError> {
        let handler = RpcHandler::new(self.clone());
        let admitted = handler
            .read_admission(request, self.key.public_key())
            .await?;
        readable(&admitted.store).await?;
        handler
            .authorize_request(request, self.key.public_key())
            .await?;
        let checkout = handler
            .checkout_access(request, admitted.repo_id, self.key.public_key())
            .await?;
        if request.ci_run.is_some() && !checkout {
            return Err(unavailable());
        }
        Ok((admitted.store, checkout, admitted.guard))
    }

    /// Connects to `node_id`, sends the request and reads the first reply. `Err` means the
    /// node gave no reply at all. A reply that refuses the session comes back in `Ok`.
    async fn open_session(
        &self,
        node_id: i64,
        request: &Request,
        deadline: tokio::time::Instant,
    ) -> Result<Opened, RepoError> {
        let opened = tokio::time::timeout_at(deadline, async {
            let mut link = self.dial(node_id, transport::ALPN_RPC).await?;
            write_frame(&mut link.writer, &serde_json::to_vec(request)?, MAX_JSON).await?;
            let reply: Reply =
                serde_json::from_slice(&read_frame(&mut link.reader, MAX_JSON).await?)?;
            Ok::<_, anyhow::Error>((link, reply))
        })
        .await;
        match opened {
            Ok(Ok(opened)) => Ok(opened),
            _ => Err(unavailable()),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct WriteLimits {
    pub(super) timeout: Duration,
    pack_bytes: u64,
}

impl WriteLimits {
    pub(super) async fn load(pool: &db::DbPool) -> Result<Self> {
        let settings =
            tokio::time::timeout(Duration::from_secs(5), journal::write_settings(pool)).await??;
        ensure!(settings.write_quorum.is_some(), "write quorum is disabled");
        let pack_bytes = match std::env::var("WRITE_QUORUM_MAX_PACK_BYTES") {
            Ok(value) => value
                .parse::<u64>()
                .context("invalid WRITE_QUORUM_MAX_PACK_BYTES")?,
            Err(std::env::VarError::NotPresent) => MAX_BYTES as u64,
            Err(error) => return Err(error.into()),
        };
        ensure!(
            (1..=1024 * 1024 * 1024).contains(&pack_bytes),
            "invalid write pack limit"
        );
        ensure!(
            (1..=120).contains(&settings.write_timeout_seconds),
            "invalid write timeout"
        );
        Ok(Self {
            timeout: Duration::from_secs(settings.write_timeout_seconds as u64),
            pack_bytes,
        })
    }
}

struct LimitedWriter<'a> {
    output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    remaining: u64,
}

impl<'a> LimitedWriter<'a> {
    fn new(output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send), limit: u64) -> Self {
        Self {
            output,
            remaining: limit,
        }
    }
}

impl tokio::io::AsyncWrite for LimitedWriter<'_> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if bytes.len() as u64 > self.remaining {
            return std::task::Poll::Ready(Err(std::io::Error::other("write pack exceeds limit")));
        }
        match std::pin::Pin::new(&mut *self.output).poll_write(cx, bytes) {
            std::task::Poll::Ready(Ok(count)) => {
                self.remaining -= count as u64;
                std::task::Poll::Ready(Ok(count))
            }
            result => result,
        }
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.output).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.output).poll_shutdown(cx)
    }
}

fn write_targets(write: &WriteRecord) -> Result<Vec<String>> {
    journal::validate_changes(&write.changes)?;
    ensure!(
        write
            .changes
            .iter()
            .all(|change| change.kind == "oid" && change.name != "HEAD"),
        "push requires oid refs"
    );
    let mut targets: Vec<_> = write
        .changes
        .iter()
        .flat_map(|change| change.old_target.iter().chain(change.new_target.iter()))
        .cloned()
        .collect();
    targets.sort();
    targets.dedup();
    Ok(targets)
}

fn preparation_path(path: &Path, write: &WriteRecord) -> Result<PathBuf> {
    journal::validate_id(&write.id)?;
    Ok(path.join("codeyard-writes").join(&write.id))
}

async fn create_preparation(path: &Path, write: &WriteRecord) -> Result<PathBuf> {
    let directory = preparation_path(path, write)?;
    let created = directory.clone();
    tokio::task::spawn_blocking(move || crate::backend::fs::private_dir(&created)).await??;
    Ok(directory)
}

fn pin_names(write: &WriteRecord) -> Result<Vec<(String, String)>> {
    Ok(write_targets(write)?
        .into_iter()
        .enumerate()
        .map(|(index, target)| (format!("refs/codeyard/writes/{}/{index}", write.id), target))
        .collect())
}

async fn write_refs(path: &Path, script: &str, limit: Duration) -> Result<()> {
    let mut command = git_process::git_command()?;
    command
        .arg("--git-dir")
        .arg(path)
        .args(["update-ref", "--stdin"]);
    let result = git_process::run(command, script.as_bytes(), 4096, limit).await?;
    ensure!(result.status.success(), "write refs could not be published");
    Ok(())
}

// Sync only the files and ancestors this operation changed, never a repository walk.
async fn sync_refs(path: &Path, names: Vec<String>) -> Result<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut directories = std::collections::BTreeSet::new();
        directories.insert(path.clone());
        for name in names
            .into_iter()
            .chain(std::iter::once("packed-refs".into()))
        {
            let file = path.join(name);
            match std::fs::File::open(&file) {
                Ok(file) => file.sync_all()?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let mut parent = file.parent();
            while let Some(directory) = parent.filter(|directory| directory.starts_with(&path)) {
                directories.insert(directory.to_owned());
                parent = directory.parent();
            }
        }
        for directory in directories.into_iter().rev() {
            match std::fs::File::open(directory) {
                Ok(file) => file.sync_all()?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

fn write_manifest(write: &WriteRecord) -> Result<Vec<u8>> {
    let manifest = serde_json::to_vec(&serde_json::json!({
        "id": write.id, "repository_id": write.repository_id, "term": write.term,
        "manifest_hash": write.manifest_hash, "changes": write.changes,
    }))?;
    ensure!(manifest.len() <= MAX_JSON, "write manifest exceeds limit");
    Ok(manifest)
}

async fn prepared_before(path: &Path, write: &WriteRecord) -> Result<bool> {
    let manifest = preparation_path(path, write)?.join("manifest.json");
    let file = match tokio::fs::File::open(manifest).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let mut data = Vec::new();
    file.take((MAX_JSON + 1) as u64)
        .read_to_end(&mut data)
        .await?;
    ensure!(
        data == write_manifest(write)?,
        "prepared write identity mismatch"
    );
    Ok(true)
}

/// Strictly index the complete retained pack before pinning any proposed object.
async fn retain_write(path: &Path, write: &WriteRecord, limits: WriteLimits) -> Result<()> {
    let directory = preparation_path(path, write)?;
    let mut input = tokio::fs::File::open(directory.join("objects.pack")).await?;
    ensure!(
        input.metadata().await?.len() <= limits.pack_bytes,
        "write pack exceeds limit"
    );
    let mut command = git_process::git_command()?;
    command
        .arg("--git-dir")
        .arg(path)
        .args(["index-pack", "--stdin", "--strict"]);
    let mut result = Vec::new();
    let mut output = LimitedWriter::new(&mut result, 256);
    let status = git_process::stream(
        command,
        &mut input,
        &mut output,
        &mut tokio::io::sink(),
        false,
    )
    .await?;
    ensure!(status.success(), "write pack failed strict indexing");
    let hash = std::str::from_utf8(&result)?
        .split_whitespace()
        .last()
        .context("missing pack hash")?;
    ensure!(
        crate::backend::git::is_full_commit_id(hash),
        "invalid pack hash"
    );
    for suffix in ["pack", "idx"] {
        tokio::fs::File::open(
            path.join("objects/pack")
                .join(format!("pack-{hash}.{suffix}")),
        )
        .await?
        .sync_all()
        .await?;
    }
    for directory in [
        path.join("objects/pack"),
        path.join("objects"),
        path.to_owned(),
    ] {
        tokio::fs::File::open(directory).await?.sync_all().await?;
    }
    let pins = pin_names(write)?;
    let mut script = String::from("start\n");
    for (name, target) in &pins {
        script.push_str(&format!("update {name} {target}\n"));
    }
    script.push_str("prepare\ncommit\n");
    write_refs(path, &script, limits.timeout).await?;
    sync_refs(path, pins.into_iter().map(|(name, _)| name).collect()).await?;
    let manifest = write_manifest(write)?;
    let target = directory.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new_in(&target)?;
        file.write_all(&manifest)?;
        file.as_file().sync_all()?;
        file.persist(target.join("manifest.json"))?;
        Ok(())
    })
    .await??;
    let parent = path.join("codeyard-writes");
    for directory in [directory.as_path(), parent.as_path(), path] {
        tokio::fs::File::open(directory).await?.sync_all().await?;
    }
    Ok(())
}

impl NodeContext {
    async fn current_write(&self, id: &str) -> Result<WriteRecord> {
        journal::validate_id(id)?;
        let pool = self.database()?;
        let write = journal::load_write(pool, id)
            .await?
            .context("write disappeared")?;
        let view = journal::recovery_view(pool, write.repository_id).await?;
        ensure!(
            view.term == write.term
                && view.primary_id == write.primary_id
                && view
                    .voters
                    .iter()
                    .any(|voter| voter.node_id == self.node_id && voter.term == write.term),
            "write term changed"
        );
        ensure!(
            db::cluster::get_node_by_id(pool, self.node_id)
                .await?
                .is_some_and(|node| node.status == "active"
                    && node.public_key == self.key.public_hex()
                    && node.roles.iter().any(|role| role == "storage")),
            "storage node is not active"
        );
        write_targets(&write)?;
        Ok(write)
    }

    async fn vote_prepared_write(&self, write: &WriteRecord) -> Result<()> {
        journal::vote_write(self.database()?, &write.id, self.node_id, write.term).await?;
        let voted = journal::load_write(self.database()?, &write.id)
            .await?
            .context("write disappeared")?;
        ensure!(
            voted.decision == "pending"
                && voted.votes.iter().any(|vote| vote.node_id == self.node_id),
            "write vote refused"
        );
        Ok(())
    }

    pub(super) async fn prepare_local_write(
        &self,
        path: &Path,
        write: &WriteRecord,
        limits: WriteLimits,
    ) -> Result<()> {
        let current = self.current_write(&write.id).await?;
        ensure!(
            current.primary_id == self.node_id && current.decision == "pending",
            "write is not pending here"
        );
        let directory = create_preparation(path, write).await?;
        if prepared_before(path, write).await? {
            retain_write(path, write, limits).await?;
            return self.vote_prepared_write(write).await;
        }
        let mut pack = tokio::fs::File::create(directory.join("objects.pack")).await?;
        let store =
            RepoStore::from_disk_path(path.to_str().context("repository path is not text")?);
        let mut output = LimitedWriter::new(&mut pack, limits.pack_bytes);
        let status = store
            .replica_pack(
                &write_targets(write)?,
                &[],
                GitStreams {
                    input: &mut tokio::io::empty(),
                    output: &mut output,
                    errors: &mut tokio::io::sink(),
                },
            )
            .await?;
        ensure!(status.success(), "write pack could not be prepared");
        pack.sync_all().await?;
        retain_write(path, write, limits).await?;
        self.vote_prepared_write(write).await
    }

    pub(super) async fn publish_local_write(&self, path: &Path, write: &WriteRecord) -> Result<()> {
        let current = self.current_write(&write.id).await?;
        ensure!(current.decision == "commit", "write is not committed");
        let owned_path = path.to_owned();
        let changes = current.changes.clone();
        tokio::task::spawn_blocking(move || {
            let repo = git2::Repository::open_bare(owned_path)?;
            for change in changes {
                let actual = match repo.find_reference(&change.name) {
                    Ok(reference) => Some(
                        reference
                            .target()
                            .context("unexpected symbolic ref")?
                            .to_string(),
                    ),
                    Err(error) if error.code() == git2::ErrorCode::NotFound => None,
                    Err(error) => return Err(error.into()),
                };
                ensure!(
                    actual == change.new_target,
                    "committed refs have not published"
                );
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        sync_refs(
            path,
            current
                .changes
                .iter()
                .map(|change| change.name.clone())
                .collect(),
        )
        .await?;
        ensure!(
            journal::mark_write_applied(
                self.database()?,
                write.repository_id,
                self.node_id,
                write.term,
                write.committed_version.context("write has no version")?
            )
            .await?,
            "write term changed during publication"
        );
        Ok(())
    }

    pub(super) async fn remove_write_preparation(
        &self,
        path: &Path,
        write: &WriteRecord,
        limits: WriteLimits,
    ) -> Result<()> {
        let pins = pin_names(write)?;
        let mut script = String::from("start\n");
        // Missing pins are harmless on an operation that never finished preparing.
        let owned_path = path.to_owned();
        let present = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String)>> {
            let repo = git2::Repository::open_bare(owned_path)?;
            let mut present = Vec::new();
            for (name, target) in pins {
                match repo.find_reference(&name) {
                    Ok(reference) => {
                        ensure!(
                            reference.target().map(|oid| oid.to_string()).as_deref()
                                == Some(target.as_str()),
                            "preparation pin changed"
                        );
                        present.push((name, target));
                    }
                    Err(error) if error.code() == git2::ErrorCode::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(present)
        })
        .await??;
        for (name, target) in &present {
            script.push_str(&format!("delete {name} {target}\n"));
        }
        script.push_str("prepare\ncommit\n");
        write_refs(path, &script, limits.timeout).await?;
        sync_refs(path, present.into_iter().map(|(name, _)| name).collect()).await?;
        match tokio::fs::remove_dir_all(preparation_path(path, write)?).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        match tokio::fs::File::open(path.join("codeyard-writes")).await {
            Ok(parent) => parent.sync_all().await?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

impl RpcHandler {
    pub(super) async fn prepare_write(
        &self,
        store: &RepoStore,
        id: &str,
        limits: WriteLimits,
    ) -> Result<()> {
        let path = store.local_path_for_spawning_git()?;
        let held = crate::backend::git::locks::publication(path).await?;
        held.scope(async {
            let write = self.context.current_write(id).await?;
            ensure!(
                write.decision == "pending" && write.primary_id != self.context.node_id,
                "write is not pending on a follower"
            );
            let directory = create_preparation(path, &write).await?;
            if prepared_before(path, &write).await? {
                retain_write(path, &write, limits).await?;
                return self.context.vote_prepared_write(&write).await;
            }
            let mut pack = tokio::fs::File::create(directory.join("objects.pack")).await?;
            let repo = db::repositories::records::get_repository_by_id(
                self.context.database()?,
                write.repository_id,
            )
            .await?
            .context("repository disappeared")?;
            let repository = super::repository::repository_uuid(&repo.disk_path)?;
            let mut output = LimitedWriter::new(&mut pack, limits.pack_bytes);
            let (_, status) = self
                .context
                .git_session(
                    write.primary_id,
                    Request {
                        repository,
                        operation: Operation::WritePack {
                            id: write.id.clone(),
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
            ensure!(status.success(), "write pack transfer failed");
            pack.sync_all().await?;
            retain_write(path, &write, limits).await?;
            self.context.vote_prepared_write(&write).await
        })
        .await
    }

    pub(super) async fn apply_write(
        &self,
        store: &RepoStore,
        id: &str,
        limits: WriteLimits,
    ) -> Result<()> {
        let path = store.local_path_for_spawning_git()?;
        let held = crate::backend::git::locks::publication(path).await?;
        held.scope(async {
            let write = self.context.current_write(id).await?;
            if write.decision == "abort" {
                return self
                    .context
                    .remove_write_preparation(path, &write, limits)
                    .await;
            }
            ensure!(write.decision == "commit", "write is not committed");
            let view =
                journal::recovery_view(self.context.database()?, write.repository_id).await?;
            let version = write.committed_version.context("write has no version")?;
            let applied = view
                .voters
                .iter()
                .find(|voter| voter.node_id == self.context.node_id)
                .context("voter disappeared")?
                .applied_version;
            if applied < version {
                ensure!(
                    applied == write.base_version,
                    "voter is not at the write's base version"
                );
                retain_write(path, &write, limits).await?;
                apply_refs(path, &write, limits.timeout).await?;
                self.context.publish_local_write(path, &write).await?;
            }
            self.context
                .remove_write_preparation(path, &write, limits)
                .await
        })
        .await
    }
}

async fn apply_refs(path: &Path, write: &WriteRecord, limit: Duration) -> Result<()> {
    let owned_path = path.to_owned();
    let changes = write.changes.clone();
    let script = tokio::task::spawn_blocking(move || -> Result<String> {
        let repo = git2::Repository::open_bare(owned_path)?;
        let mut script = String::from("start\n");
        for change in changes {
            let actual = match repo.find_reference(&change.name) {
                Ok(reference) => Some(
                    reference
                        .target()
                        .context("unexpected symbolic ref")?
                        .to_string(),
                ),
                Err(error) if error.code() == git2::ErrorCode::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if actual == change.new_target {
                continue;
            }
            ensure!(
                actual == change.old_target,
                "write reference old value mismatch"
            );
            let old = change
                .old_target
                .as_deref()
                .unwrap_or("0000000000000000000000000000000000000000");
            match change.new_target {
                Some(new) => script.push_str(&format!("update {} {new} {old}\n", change.name)),
                None => script.push_str(&format!("delete {} {old}\n", change.name)),
            }
        }
        script.push_str("prepare\ncommit\n");
        Ok(script)
    })
    .await??;
    write_refs(path, &script, limit).await
}

/// Check Git before admission, while failover can still try another node.
async fn readable(store: &RepoStore) -> Result<(), RepoError> {
    let path = store.local_path_for_spawning_git()?.to_owned();
    let opens = git_process::blocking_until(std::time::Instant::now() + TIMEOUT, move || {
        crate::backend::git::repository::opens_as_repository(&path)
    })
    .await
    .map_err(|_| unavailable())?;
    if opens {
        Ok(())
    } else {
        Err(unavailable())
    }
}

impl RpcHandler {
    pub(super) async fn serve_session(
        &self,
        request: Request,
        mut link: Link,
        deadline: tokio::time::Instant,
        admission: (
            tokio::sync::OwnedSemaphorePermit,
            tokio::sync::OwnedSemaphorePermit,
        ),
    ) -> Result<()> {
        let caller = link.peer;
        let write_pack = matches!(request.operation, Operation::WritePack { .. });
        let deadline = if write_pack {
            deadline.min(
                tokio::time::Instant::now()
                    + WriteLimits::load(self.context.database()?).await?.timeout,
            )
        } else {
            deadline
        };
        let prepared = tokio::time::timeout_at(deadline, async {
            self.authorize_request(&request, caller).await?;
            // A receive waiting for its voter already owns a session permit. Packs use
            // the existing bounded header admission instead, avoiding that circular wait.
            let permit = if write_pack { None } else { Some(git_process::session_permit().await.map_err(RepoError::from)?) };
            let pack_admission = if write_pack { Some(admission) } else { drop(admission); None };
            // The wait for the slot can outlast the membership or copy this request was
            // admitted on, so the same check runs again now.
            let (store, repo_id, read_guard) = if request.operation.is_read_only() {
                let admitted = self.read_admission(&request, caller).await?;
                (admitted.store, admitted.repo_id, Some(admitted.guard))
            } else {
                let (store, repo_id) = self.authorize_request(&request, caller).await?;
                (store, repo_id, None)
            };
            readable(&store).await?;
            self.authorize_request(&request, caller).await?;
            let checkout = self.checkout_access(&request, repo_id, caller).await?;
            if request.ci_run.is_some() && !checkout { return Err(unavailable()); }
            let (admission, targets) = match &request.operation {
                // The copy learns HEAD and every ref now. The pack covers exactly these
                // refs, so a copy never points a ref at objects it does not have.
                Operation::WritePack { id } => {
                    let write = journal::load_write(self.context.database()?, id).await.map_err(RepoError::from)?.ok_or_else(unavailable)?;
                    (Value::Null, write_targets(&write).map_err(RepoError::from)?)
                }
                Operation::ReplicaPack { .. } => {
                    let (refs, head) = store.replica_listing().await?;
                    let mut targets: Vec<String> = refs.values().cloned().collect();
                    let retained = crate::backend::db::ci::retained_ci_commits(self.context.database()?, repo_id.ok_or_else(unavailable)?).await.map_err(RepoError::from)?;
                    let present = store.existing_commits(&retained).await?;
                    targets.extend(retained.into_iter().filter(|commit| present.contains(commit)));
                    targets.extend(crate::backend::db::pull_requests::records::retained_revisions(self.context.database()?, repo_id.ok_or_else(unavailable)?).await.map_err(RepoError::from)?);
                    targets.sort();
                    targets.dedup();
                    (serde_json::json!({ "head": head, "refs": refs }), targets)
                }
                _ => (Value::Null, Vec::new()),
            };
            let receive = if matches!(&request.operation, Operation::GitSession(session) if session.service == GitService::ReceivePack) {
                let pool = self.context.database()?.clone();
                let ci = Arc::new(CiRuns::new(
                    pool.clone(),
                    Arc::new(CiConfig::from_env().map_err(RepoError::from)?),
                    Arc::new(tokio::sync::Notify::new()),
                    Some(self.context.clone()),
                    Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                        &pool,
                    )),
                ));
                Some((pool, ci))
            } else {
                None
            };
            Ok::<_, RepoError>((store, admission, targets, receive, permit, checkout, read_guard, pack_admission))
        })
        .await.unwrap_or_else(|_| Err(unavailable()));
        let (store, admission, targets, receive, _permit, checkout, _read_guard, _pack_admission) =
            match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    super::refuse(&mut link, error).await?;
                    return Ok(());
                }
            };
        // The permit is held while the reply is written, so the write has a deadline too.
        tokio::time::timeout_at(
            deadline,
            write_frame(
                &mut link.writer,
                &serde_json::to_vec(&Reply {
                    result: Ok(admission),
                })?,
                MAX_JSON,
            ),
        )
        .await??;
        let (mut output, mut reader) = tokio::io::duplex(git_process::CHUNK);
        let receiving = receive.is_some();
        let input = &mut link.reader;
        let run = async move {
            let mut errors = git_process::StderrTail::default();
            let streams = GitStreams {
                input,
                output: &mut output,
                errors: &mut errors,
            };
            let status = match request.operation {
                Operation::GitSession(mut session) => {
                    session.allow_unreachable = checkout;
                    store.git_session(session, receive, streams).await
                }
                Operation::ReplicaPack { have } => {
                    store.replica_pack(&targets, &have, streams).await
                }
                Operation::WritePack { .. } => {
                    async {
                        let limits = WriteLimits::load(self.context.database()?).await?;
                        let mut bounded = LimitedWriter::new(streams.output, limits.pack_bytes);
                        tokio::time::timeout(
                            limits.timeout,
                            store.replica_pack(
                                &targets,
                                &[],
                                GitStreams {
                                    input: streams.input,
                                    output: &mut bounded,
                                    errors: streams.errors,
                                },
                            ),
                        )
                        .await?
                    }
                    .await
                }
                _ => unreachable!("only streamed operations are served here"),
            };
            let result = status
                .map(|status| status.code().unwrap_or(1))
                .map_err(RepoError::from);
            GitExit {
                result,
                stderr: errors.text(),
            }
        };
        let outbound = {
            let send = &mut link.writer;
            // The block owns the reader, so returning from it makes Git's next write fail.
            async move {
                let mut buffer = vec![0; git_process::CHUNK];
                let mut disconnected = false;
                loop {
                    let count = reader.read(&mut buffer).await?;
                    if count == 0 {
                        break;
                    }
                    if !disconnected {
                        disconnected = !matches!(
                            tokio::time::timeout(
                                git_process::IDLE_TIMEOUT,
                                write_frame(send, &buffer[..count], git_process::CHUNK)
                            )
                            .await,
                            Ok(Ok(()))
                        );
                        // A lost upload-pack client stops the process. A receive keeps
                        // draining, because its hooks must finish.
                        if disconnected && !receiving {
                            break;
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }
        };
        let (reply, _) = tokio::join!(run, outbound);
        // Empty stdout frame separates binary chunks from the final JSON status. A peer
        // that stopped reading must not hold the session permit for ever.
        tokio::time::timeout(git_process::IDLE_TIMEOUT, async {
            write_frame(&mut link.writer, b"", git_process::CHUNK).await?;
            write_frame(&mut link.writer, &serde_json::to_vec(&reply)?, MAX_JSON).await?;
            link.finish().await?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("the caller stopped reading the session result"))?
    }
}

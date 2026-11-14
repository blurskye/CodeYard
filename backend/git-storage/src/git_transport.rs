use crate::context::NodeContext;
use crate::repositories::store::RepoStore;
use crate::rpc_client::attempt_deadline;
use crate::rpc_client::cannot_serve;
use crate::rpc_client::log_failover;
use crate::store_types::GitService;
use crate::store_types::GitStreams;
use crate::wire::unavailable;
use crate::wire::Failure;
use crate::wire::GitExit;
use crate::wire::Operation;
use crate::wire::Reply;
use crate::wire::Request;
use crate::wire::MAX_BYTES;
use crate::wire::TIMEOUT;
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use codeyard_database::limits::MAX_JSON;
use codeyard_database::models::RefChange;
use codeyard_database::models::WriteRecord;
use codeyard_git::error::RepoError;
use codeyard_git::repository::process as git_process;
use codeyard_nodes::link::read_frame;
use codeyard_nodes::link::write_frame;
use codeyard_nodes::link::Link;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

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
        let read_only = request.operation.is_read_only();
        let mut candidates = vec![(node_id, true)];
        if read_only {
            candidates.extend(
                tokio::time::timeout_at(deadline, self.read_copies(request.repository, node_id))
                    .await
                    .map_err(|_| unavailable())?,
            );
            // User clones share the primary and current copies. Snapshot and CI transfers
            // retain their primary-first order and pinned-commit admission.
            if request.ci_run.is_none()
                && matches!(&request.operation, Operation::GitSession(session) if session.service == GitService::UploadPack)
            {
                let offset = self
                    .clone_cursor
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    % candidates.len();
                candidates.rotate_left(offset);
            }
        }
        let mut opened = Err(unavailable());
        let mut candidates = candidates.into_iter().peekable();
        while let Some((candidate, primary)) = candidates.next() {
            let candidate_deadline = if candidates.peek().is_none() {
                deadline
            } else {
                attempt_deadline(deadline)
            };
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            if candidate == self.node_id && read_only {
                // Admission repeats current-copy and live credential checks after waiting
                // for the permit, before touching the client's streams.
                let Ok(Ok(_permit)) =
                    tokio::time::timeout_at(candidate_deadline, git_process::session_permit())
                        .await
                else {
                    continue;
                };
                if let Ok(Ok((store, checkout, _read_guard))) =
                    tokio::time::timeout_at(candidate_deadline, self.own_copy(&request)).await
                {
                    let Operation::GitSession(mut session) = request.operation else {
                        return Err(unavailable().into());
                    };
                    session.allow_unreachable = checkout;
                    let status = Box::pin(store.git_session(session, None, streams)).await?;
                    return Ok((Value::Null, status));
                }
                continue;
            }
            let answer = self
                .open_session(candidate, &request, candidate_deadline)
                .await;
            if !read_only || !cannot_serve(answer.as_ref().ok().map(|(_, reply)| reply), primary) {
                if candidate != node_id {
                    log_failover(request.repository, node_id, candidate);
                }
                opened = answer;
                break;
            }
            opened = answer;
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

    async fn own_copy(
        &self,
        request: &Request,
    ) -> Result<(RepoStore, bool, tokio::sync::OwnedRwLockReadGuard<()>), RepoError> {
        self.rpc.create(self.clone()).own_copy(request).await
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
            let mut link = self
                .dial(node_id, codeyard_nodes::transport::ALPN_RPC)
                .await?;
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
pub struct WriteLimits {
    pub timeout: Duration,
    pub pack_bytes: u64,
}

impl WriteLimits {
    pub async fn load(pool: &codeyard_database::DbPool) -> Result<Self> {
        let settings = tokio::time::timeout(
            Duration::from_secs(5),
            codeyard_db_placement::cluster::writes::write_settings(pool),
        )
        .await??;
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

pub struct LimitedWriter<'a> {
    output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    remaining: u64,
}

impl<'a> LimitedWriter<'a> {
    pub fn new(output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send), limit: u64) -> Self {
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

pub fn write_targets(write: &WriteRecord) -> Result<Vec<String>> {
    codeyard_db_placement::cluster::writes::validate_changes(&write.changes)?;
    ensure!(
        write
            .changes
            .iter()
            .all(|change| (change.kind == "oid" && change.name != "HEAD")
                || (change.kind == "symbolic"
                    && change.name == "HEAD"
                    && change.new_target.is_some())),
        "unsupported write reference kind"
    );
    let mut targets: Vec<_> = write
        .changes
        .iter()
        .filter(|change| change.kind == "oid")
        .flat_map(|change| change.old_target.iter().chain(change.new_target.iter()))
        .cloned()
        .collect();
    targets.sort();
    targets.dedup();
    Ok(targets)
}

/// The distinct commits at a voter's refs. A voter votes only at the write's base version,
/// so it already holds their history and a write pack leaves it out, as a fetch does.
pub async fn voter_tips(path: &Path, limits: WriteLimits) -> Result<Vec<String>> {
    let path = path.to_owned();
    let mut tips: Vec<String> =
        git_process::blocking_until(std::time::Instant::now() + limits.timeout, move || {
            codeyard_git::repository::list_mirror_refs(&path)
        })
        .await?
        .into_values()
        .collect();
    tips.sort();
    tips.dedup();
    Ok(tips)
}

pub fn preparation_path(path: &Path, write: &WriteRecord) -> Result<PathBuf> {
    codeyard_db_placement::cluster::writes::validate_id(&write.id)?;
    Ok(path.join("codeyard-writes").join(&write.id))
}

pub async fn create_preparation(path: &Path, write: &WriteRecord) -> Result<PathBuf> {
    let directory = preparation_path(path, write)?;
    let created = directory.clone();
    tokio::task::spawn_blocking(move || codeyard_security::fs::private_dir(&created)).await??;
    Ok(directory)
}

pub fn pin_names(write: &WriteRecord) -> Result<Vec<(String, String)>> {
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

fn write_manifest(write: &WriteRecord) -> Result<Vec<u8>> {
    let manifest = serde_json::to_vec(&serde_json::json!({
        "id": write.id, "repository_id": write.repository_id, "term": write.term,
        "manifest_hash": write.manifest_hash, "changes": write.changes,
    }))?;
    ensure!(manifest.len() <= MAX_JSON, "write manifest exceeds limit");
    Ok(manifest)
}

pub async fn prepared_before(path: &Path, write: &WriteRecord) -> Result<bool> {
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

// Git's native rename handles file/directory transitions that update-ref cannot lock together.
fn branch_namespace_rename(changes: &[RefChange]) -> Option<(&RefChange, &RefChange)> {
    let mut branches = changes
        .iter()
        .filter(|change| change.kind == "oid" && change.name.starts_with("refs/heads/"));
    let (first, second) = (branches.next()?, branches.next()?);
    if branches.next().is_some() {
        return None;
    }
    let (old, new) = if first.new_target.is_none() {
        (first, second)
    } else {
        (second, first)
    };
    (old.new_target.is_none()
        && old.old_target.is_some()
        && new.old_target.is_none()
        && new.new_target == old.old_target
        && (old.name.starts_with(&format!("{}/", new.name))
            || new.name.starts_with(&format!("{}/", old.name))))
    .then_some((old, new))
}

/// Strictly index the retained pack before pinning any proposed object. Strict indexing
/// also checks that history left out of the pack is present here.
pub async fn retain_write(path: &Path, write: &WriteRecord, limits: WriteLimits) -> Result<()> {
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
    ensure!(codeyard_git::is_full_commit_id(hash), "invalid pack hash");
    // The pack and index bytes are durable; the entries that name them are not. A voter keeps
    // its pack past a crash only after this barrier.
    let mut published = codeyard_git::repository::durable::RepositorySync::new().objects();
    for suffix in ["pack", "idx"] {
        published = published.required_file(format!("objects/pack/pack-{hash}.{suffix}"));
    }
    published
        .sync(path, std::time::Instant::now() + limits.timeout)
        .await?;
    let repository = path.to_owned();
    let changes = write.changes.clone();
    let committed = write.decision == "commit";
    git_process::blocking_until(std::time::Instant::now() + limits.timeout, move || {
        let repo = git2::Repository::open_bare(repository)?;
        let renamed = branch_namespace_rename(&changes).map(|(old, _)| old.name.as_str());
        let mut created = Vec::new();
        for change in &changes {
            let actual = match repo.find_reference(&change.name) {
                Ok(reference) => Some(reference_target(&reference, &change.kind)?),
                Err(error) if error.code() == git2::ErrorCode::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            ensure!(
                actual == change.old_target || (committed && actual == change.new_target),
                "voter reference old value mismatch"
            );
            if change.kind == "oid" {
                if let Some(target) = &change.new_target {
                    if change.name.starts_with("refs/heads/") {
                        let object = repo.find_object(git2::Oid::from_str(target)?, None)?;
                        ensure!(
                            object.kind() == Some(git2::ObjectType::Commit),
                            "branch target must be a commit"
                        );
                    }
                    if change.old_target.is_none() && actual.is_none() {
                        let mut parent = change.name.as_str();
                        while let Some((prefix, _)) = parent.rsplit_once('/') {
                            parent = prefix;
                            if !parent.starts_with("refs/") {
                                break;
                            }
                            match repo.find_reference(parent) {
                                Ok(_) if renamed == Some(parent) => {}
                                Ok(_) => anyhow::bail!("reference namespace conflict"),
                                Err(error) if error.code() == git2::ErrorCode::NotFound => {}
                                Err(error) => return Err(error.into()),
                            }
                        }
                        for reference in
                            repo.references_glob(&format!("{}/*", change.name))?.take(2)
                        {
                            ensure!(
                                reference?.name().is_ok_and(|name| renamed == Some(name)),
                                "reference namespace conflict"
                            );
                        }
                        created.push(&change.name);
                    }
                }
            }
        }
        created.sort();
        for names in created.windows(2) {
            ensure!(
                !names[1]
                    .strip_prefix(names[0].as_str())
                    .is_some_and(|tail| tail.starts_with('/')),
                "reference namespace conflict"
            );
        }
        Ok(())
    })
    .await?;
    let pins = pin_names(write)?;
    let mut script = String::from("start\n");
    for (name, target) in &pins {
        script.push_str(&format!("update {name} {target}\n"));
    }
    script.push_str("prepare\ncommit\n");
    write_refs(path, &script, limits.timeout).await?;
    codeyard_git::repository::durable::sync_refs(
        path,
        pins.into_iter().map(|(name, _)| name).collect(),
        std::time::Instant::now() + limits.timeout,
    )
    .await?;
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
    pub(crate) async fn import_write_objects(
        &self,
        source: &Path,
        path: &Path,
        targets: &[String],
    ) -> Result<()> {
        let limits = WriteLimits::load(self.database()?).await?;
        tokio::time::timeout(limits.timeout, async {
            let temporary = tempfile::NamedTempFile::new()?;
            let source =
                RepoStore::from_disk_path(source.to_str().context("invalid preparation path")?);
            let destination =
                RepoStore::from_disk_path(path.to_str().context("invalid repository path")?);
            let mut pack = tokio::fs::File::create(temporary.path()).await?;
            let mut output = LimitedWriter::new(&mut pack, limits.pack_bytes);
            let status = source
                .replica_pack(
                    targets,
                    &[],
                    GitStreams {
                        input: &mut tokio::io::empty(),
                        output: &mut output,
                        errors: &mut tokio::io::sink(),
                    },
                )
                .await?;
            ensure!(status.success(), "could not prepare publication objects");
            drop(pack);
            destination.index_pack(temporary.path(), true).await?;
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    pub async fn current_write(&self, id: &str) -> Result<WriteRecord> {
        codeyard_db_placement::cluster::writes::validate_id(id)?;
        let pool = self.database()?;
        let write = codeyard_db_placement::cluster::writes::load_write(pool, id)
            .await?
            .context("write disappeared")?;
        let view = codeyard_db_placement::cluster::writes::recovery_view(pool, write.repository_id)
            .await?;
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
            codeyard_db_placement::cluster::get_node_by_id(pool, self.node_id)
                .await?
                .is_some_and(|node| node.status == "active"
                    && node.public_key == self.key.public_hex()
                    && node.roles.iter().any(|role| role == "storage")),
            "storage node is not active"
        );
        write_targets(&write)?;
        Ok(write)
    }

    pub async fn vote_prepared_write(&self, write: &WriteRecord) -> Result<()> {
        codeyard_db_placement::cluster::writes::vote_write(
            self.database()?,
            &write.id,
            self.node_id,
            write.term,
        )
        .await?;
        let voted = codeyard_db_placement::cluster::writes::load_write(self.database()?, &write.id)
            .await?
            .context("write disappeared")?;
        ensure!(
            voted.decision == "pending"
                && voted.votes.iter().any(|vote| vote.node_id == self.node_id),
            "write vote refused"
        );
        Ok(())
    }

    pub async fn prepare_local_write(
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
        let have = voter_tips(path, limits).await?;
        let mut pack = tokio::fs::File::create(directory.join("objects.pack")).await?;
        let store =
            RepoStore::from_disk_path(path.to_str().context("repository path is not text")?);
        let mut output = LimitedWriter::new(&mut pack, limits.pack_bytes);
        let status = store
            .replica_pack(
                &write_targets(write)?,
                &have,
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

    pub async fn publish_local_write(&self, path: &Path, write: &WriteRecord) -> Result<()> {
        let current = self.current_write(&write.id).await?;
        ensure!(current.decision == "commit", "write is not committed");
        let owned_path = path.to_owned();
        let changes = current.changes.clone();
        tokio::task::spawn_blocking(move || {
            let repo = git2::Repository::open_bare(owned_path)?;
            for change in changes {
                let actual = match repo.find_reference(&change.name) {
                    Ok(reference) => Some(reference_target(&reference, &change.kind)?),
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
        // The deleted name may now traverse a ref file rather than a directory.
        // Syncing the new name and packed-refs covers the namespace replacement.
        let renamed = branch_namespace_rename(&current.changes).map(|(old, _)| old.name.as_str());
        codeyard_git::repository::durable::sync_refs(
            path,
            current
                .changes
                .iter()
                .filter(|change| renamed != Some(change.name.as_str()))
                .map(|change| change.name.clone())
                .collect(),
            std::time::Instant::now() + git_process::RPC_TIMEOUT,
        )
        .await?;
        ensure!(
            codeyard_db_placement::cluster::writes::mark_write_applied(
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

    pub async fn remove_write_preparation(
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
        codeyard_git::repository::durable::sync_refs(
            path,
            present.into_iter().map(|(name, _)| name).collect(),
            std::time::Instant::now() + limits.timeout,
        )
        .await?;
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

fn reference_target(reference: &git2::Reference<'_>, kind: &str) -> Result<String> {
    if kind == "symbolic" {
        Ok(reference
            .symbolic_target()?
            .context("expected symbolic ref")?
            .to_owned())
    } else {
        Ok(reference
            .target()
            .context("unexpected symbolic ref")?
            .to_string())
    }
}

pub async fn apply_refs(path: &Path, write: &WriteRecord, limit: Duration) -> Result<()> {
    let renamed = branch_namespace_rename(&write.changes);
    if let Some((old, new)) = renamed {
        let owned_path = path.to_owned();
        let name = new.name.clone();
        let target = new.new_target.clone();
        let applied = tokio::task::spawn_blocking(move || -> Result<bool> {
            let repo = git2::Repository::open_bare(owned_path)?;
            let reference = repo.find_reference(&name);
            match reference {
                Ok(reference) => Ok(reference.target().map(|oid| oid.to_string()) == target),
                Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(false),
                Err(error) => Err(error.into()),
            }
        })
        .await??;
        if !applied {
            // The SQL decision is durable; interrupted application replays the same rename.
            tokio::time::timeout(
                limit,
                codeyard_git::publish_branch_rename(
                    path,
                    old.name
                        .strip_prefix("refs/heads/")
                        .context("invalid branch")?,
                    new.name
                        .strip_prefix("refs/heads/")
                        .context("invalid branch")?,
                    old.old_target.as_deref().context("missing rename target")?,
                ),
            )
            .await??;
        }
    }
    let renamed = renamed.map(|(old, new)| (old.name.clone(), new.name.clone()));
    let owned_path = path.to_owned();
    let changes = write.changes.clone();
    let script = tokio::task::spawn_blocking(move || -> Result<String> {
        let repo = git2::Repository::open_bare(owned_path)?;
        let mut script = String::from("start\noption no-deref\n");
        for change in changes {
            if renamed
                .as_ref()
                .is_some_and(|(old, new)| change.name == *old || change.name == *new)
            {
                continue;
            }
            let actual = match repo.find_reference(&change.name) {
                Ok(reference) => Some(reference_target(&reference, &change.kind)?),
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
            if change.kind == "symbolic" {
                // The publication lease excludes other writers, including symbolic HEAD changes.
                repo.reference_symbolic(
                    &change.name,
                    change
                        .new_target
                        .as_deref()
                        .context("symbolic deletion unsupported")?,
                    true,
                    "apply committed write",
                )?;
                continue;
            }
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
    // The objects a committed write imported must be durable before a ref names them. The
    // caller has already indexed them; this makes their entries durable first.
    codeyard_git::repository::durable::RepositorySync::new()
        .objects()
        .sync(path, std::time::Instant::now() + limit)
        .await?;
    write_refs(path, &script, limit).await?;
    // The database decision is durable before this runs, so a replica's acknowledgement must
    // not name a ref or object entry that a crash can drop.
    codeyard_git::repository::durable::sync_refs(
        path,
        write
            .changes
            .iter()
            .map(|change| change.name.clone())
            .collect(),
        std::time::Instant::now() + limit,
    )
    .await
}

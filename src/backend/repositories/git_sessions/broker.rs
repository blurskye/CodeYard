use super::protocol::{self, Request, CHUNK, EOF, STDERR, STDIN, STDOUT};
use super::validation::{self, RefUpdate};
use super::{private_files, ReceiveCredential};
use crate::backend::ci::CiRuns;
use crate::backend::db::{self, models::Repository, DbPool};
use crate::backend::git::repository::process as git_process;
use crate::backend::repositories::store::{
    begin_ref_change, repo_store, ErrorKind, GitService, GitSession, GitStreams, RepoError,
};
use anyhow::{ensure, Context, Result};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio::time::timeout;

const MAX_HEADERS: usize = 32;
const MAX_CONTROLS: usize = 64;

pub(super) struct Gateway {
    pub(super) socket: PathBuf,
    pub(super) repos_root: PathBuf,
    pub(super) executable: PathBuf,
    pub(super) git: PathBuf,
    pub(super) pool: DbPool,
    pub(super) ci: Arc<CiRuns>,
    pub(super) git_user: String,
    pub(super) uid: u32,
    pub(super) sessions: Arc<Semaphore>,
    pub(super) controls: Semaphore,
    pub(super) operations: Mutex<HashMap<String, Arc<Operation>>>,
}

pub(super) struct Operation {
    pub(super) credential: ReceiveCredential,
    pub(super) repo_id: i64,
    pub(super) owner: String,
    pub(super) path: PathBuf,
    // Only the broker holds the generated hook directory's lifetime.
    pub(super) hooks: TempDir,
    pub(super) state: Mutex<HookState>,
}

#[derive(Default)]
pub(super) struct HookState {
    pub(super) proposed: Option<Vec<RefUpdate>>,
    pub(super) post_started: bool,
    pub(super) dispatch_failed: bool,
    // Held from admission until Git reports publication (or the receive ends).
    pub(super) publication: Option<tokio::sync::OwnedMutexGuard<()>>,
}

struct ActiveOperation {
    gateway: Arc<Gateway>,
    token: String,
}

impl Drop for ActiveOperation {
    fn drop(&mut self) {
        self.gateway
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.token);
    }
}

pub(super) async fn run(
    socket: PathBuf,
    repos_root: PathBuf,
    pool: DbPool,
    ci: Arc<CiRuns>,
    git_user: String,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let uid = private_files::effective_uid()?;
    ensure!(
        !git_user.is_empty()
            && git_user.len() <= 64
            && git_user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
        "invalid Git system user"
    );
    let executable =
        private_files::trusted_executable(&std::fs::canonicalize(std::env::current_exe()?)?, uid)?;
    let git = private_files::git_executable(uid)?;
    let repos_root = std::fs::canonicalize(repos_root).context("repository root must exist")?;
    ensure!(repos_root.is_dir(), "repository root must be a directory");
    let (listener, _socket_guard) = private_files::bind(&socket, uid).await?;
    let gateway = Arc::new(Gateway {
        socket,
        repos_root,
        executable,
        git,
        pool,
        ci,
        git_user,
        uid,
        sessions: git_process::session_slots(),
        controls: Semaphore::new(MAX_CONTROLS),
        operations: Mutex::new(HashMap::new()),
    });
    listen(gateway, listener, stop).await
}

/// HTTP and built-in SSH use the OpenSSH pre/post hooks through a private
/// per-receive socket, without a persistent second listener or copied policy.
pub(super) async fn receive_session(
    pool: DbPool,
    ci: Arc<CiRuns>,
    receive: super::ReceiveSession<'_>,
    streams: GitStreams<'_>,
) -> Result<std::process::ExitStatus> {
    let directory = private_files::private_tempdir("git-receive-")?;
    let socket = directory.path().join("broker.sock");
    let uid = private_files::effective_uid()?;
    let executable = hook_executable(uid)?;
    let path = std::fs::canonicalize(receive.path)?;
    let gateway = Arc::new(Gateway {
        socket: socket.clone(),
        repos_root: path
            .parent()
            .context("repository parent unavailable")?
            .to_owned(),
        executable,
        git: private_files::git_executable(uid)?,
        pool,
        ci,
        git_user: "git".into(),
        uid,
        sessions: git_process::session_slots(),
        controls: Semaphore::new(2),
        operations: Mutex::new(HashMap::new()),
    });
    let (listener, _socket_guard) = private_files::bind(&socket, uid).await?;
    let repo = db::repositories::records::get_repository_by_id(&gateway.pool, receive.repo_id)
        .await?
        .context("repository unavailable")?;
    ensure!(
        gateway.bare_path(&repo).await? == path,
        "repository location changed"
    );
    let rpc = gateway.session(&repo, path, receive.session, streams);
    tokio::pin!(rpc);
    let mut hooks = JoinSet::new();
    let result = loop {
        tokio::select! {
            result = &mut rpc => break result,
            connection = listener.accept(), if hooks.len() < 2 => {
                let (mut stream, _) = connection?;
                ensure!(stream.peer_cred()?.uid() == uid, "untrusted hook peer");
                let gateway = gateway.clone();
                hooks.spawn(async move {
                    let result = async {
                        let request = protocol::read_request(&mut stream).await?;
                        ensure!(matches!(&request, Request::Hook { .. }), "hook request required");
                        gateway.handle(&mut stream, request).await
                    }.await;
                    if let Err(error) = &result {
                        if let Some(message) = refusal_message(error) {
                            let _ = protocol::write_frame(&mut stream, STDERR, &message).await;
                        }
                        let _ = protocol::finish(&mut stream, 1).await;
                    }
                });
            }
            result = hooks.join_next(), if !hooks.is_empty() => {
                result.context("hook task missing")?.context("hook task failed")?;
            }
        }
    };
    // A dead child cannot terminate in-process hook bookkeeping. Join that work
    // too before removing its token or returning the caller's admission permit.
    while let Some(result) = hooks.join_next().await {
        result.context("hook task failed")?;
    }
    result
}

fn hook_executable(uid: u32) -> Result<PathBuf> {
    // Production always runs this application's early credential-free helper.
    // Rust unit tests are not an application executable; CI supplies the real
    // integrated binary explicitly, with the same ownership checks.
    #[cfg(test)]
    let executable = PathBuf::from(
        std::env::var_os("GITSERV_TEST_OPENSSH_HELPER")
            .context("set GITSERV_TEST_OPENSSH_HELPER to the integrated application")?,
    );
    #[cfg(not(test))]
    let executable = std::env::current_exe()?;
    private_files::trusted_executable(&std::fs::canonicalize(executable)?, uid)
}

/// The text a refused hook gives the pusher. Only a deliberate refusal, a `RepoError` of kind
/// `Invalid`, has one. Other failures can name paths or driver errors, so they stay private.
fn refusal_message(error: &anyhow::Error) -> Option<Vec<u8>> {
    if error.is::<git_process::SessionsBusy>() {
        return Some(b"Git sessions busy\n".to_vec());
    }
    error
        .downcast_ref::<RepoError>()
        .filter(|error| error.kind == ErrorKind::Invalid)
        .map(|error| format!("{}\n", error.message).into_bytes())
}

/// Accepts connections until `stop` is set. After that only hook requests are served, so a
/// push that is already running can finish. Other requests are refused. The listener closes
/// when no connection is left. The caller bounds that wait.
async fn listen(
    gateway: Arc<Gateway>,
    listener: UnixListener,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let headers = Arc::new(Semaphore::new(MAX_HEADERS));
    // A live Serve connection stays open while its hook makes a second connection.
    let connections = Arc::new(Semaphore::new(connection_budget(
        git_process::session_capacity(),
    )));
    let mut tasks = JoinSet::new();
    let mut draining = false;
    // Connection tasks read the stop flag through their own receiver, because the select
    // below keeps `stop` borrowed while it waits.
    let stopping = stop.clone();
    loop {
        if draining && tasks.is_empty() {
            break;
        }
        tokio::select! {
            _ = stop.wait_for(|stopped| *stopped), if !draining => draining = true,
            connection = listener.accept() => {
                let (stream, _) = connection?;
                let peer = stream.peer_cred()?.uid();
                if peer != gateway.uid && peer != 0 { continue; }
                let Ok(connection_slot) = connections.clone().try_acquire_owned() else { continue; };
                let Ok(header_slot) = headers.clone().try_acquire_owned() else { continue; };
                let gateway = gateway.clone();
                let stopping = stopping.clone();
                tasks.spawn(async move {
                    let _connection_slot = connection_slot;
                    let mut stream = stream;
                    let request = protocol::read_request(&mut stream).await;
                    drop(header_slot);
                    if let Ok(request) = request {
                        let post = matches!(&request, Request::Hook { post: true, .. });
                        // A push that is already running still needs its hooks. Once shutdown
                        // has started, nothing else is admitted.
                        let shutting_down = *stopping.borrow();
                        let result = if shutting_down && !matches!(&request, Request::Hook { .. }) {
                            Err(anyhow::anyhow!("OpenSSH broker is shutting down"))
                        } else {
                            gateway.handle(&mut stream, request).await
                        };
                        if let Err(error) = &result {
                            // Do not disclose driver errors, paths, or arbitrary client input.
                            tracing::warn!(post_receive = post, "OpenSSH broker operation denied or failed");
                            if let Some(message) = refusal_message(error) {
                                let _ = protocol::write_frame(&mut stream, STDERR, &message).await;
                            } else {
                                let message = if post {
                                    b"Post-push metadata/CI dispatch failed; any already accepted Git writes remain accepted.\n".as_slice()
                                } else {
                                    b"OpenSSH Git operation denied or failed.\n".as_slice()
                                };
                                let _ = protocol::write_frame(&mut stream, STDERR, message).await;
                            }
                            let _ = protocol::finish(&mut stream, 1).await;
                        }
                    }
                });
            }
            result = tasks.join_next(), if !tasks.is_empty() => {
                if result.is_some_and(|result| result.is_err()) {
                    tracing::warn!("OpenSSH broker connection task failed");
                }
            }
        }
    }
    drop(listener);
    Ok(())
}

fn connection_budget(sessions: usize) -> usize {
    2 * sessions + MAX_CONTROLS + MAX_HEADERS + 16
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    #[test]
    fn connection_capacity_includes_active_receives_their_hooks_and_controls() {
        for sessions in [16, 128, 512] {
            assert!(connection_budget(sessions) >= sessions * 2 + MAX_CONTROLS + MAX_HEADERS);
        }
    }

    #[test]
    fn busy_session_refusals_keep_the_public_reason() {
        assert_eq!(
            refusal_message(&anyhow::Error::new(git_process::SessionsBusy)),
            Some(b"Git sessions busy\n".to_vec())
        );
        assert!(refusal_message(&anyhow::anyhow!("private driver detail")).is_none());
    }
}

impl Gateway {
    async fn handle(self: &Arc<Self>, stream: &mut UnixStream, request: Request) -> Result<()> {
        match request {
            Request::Serve {
                key_id,
                command,
                git_protocol,
            } => {
                // Separate budgets for Git sessions and for key/hook requests, so
                // busy Git sessions can't block the hooks they are waiting on.
                let slot =
                    git_process::wait_for_slot(self.sessions.clone(), git_process::session_wait())
                        .await?;
                // The session runs in its own task so a disconnecting client
                // can't abort a push halfway through its hooks.
                let (relay, mut owned_stream) = UnixStream::pair()?;
                let gateway = self.clone();
                let task = tokio::spawn(async move {
                    let result = gateway
                        .serve(
                            &mut owned_stream,
                            key_id,
                            &command,
                            git_protocol.as_deref(),
                            slot,
                        )
                        .await;
                    if let Err(error) = result {
                        tracing::warn!("OpenSSH Git session denied or failed");
                        let message = error
                            .downcast_ref::<RepoError>()
                            .filter(|error| error.kind == ErrorKind::Invalid)
                            .map_or_else(
                                || "OpenSSH Git operation denied or failed.\n".to_owned(),
                                |error| format!("{}\n", error.message),
                            );
                        let _ =
                            protocol::write_frame(&mut owned_stream, STDERR, message.as_bytes())
                                .await;
                        let _ = protocol::finish(&mut owned_stream, 1).await;
                    }
                });
                // The session ends when its task drops its end of the pair, whatever the
                // client is still sending. Writing to a finished session fails, and that
                // must not discard the answer still waiting to be forwarded.
                let (mut client_read, mut client_write) = stream.split();
                let (mut session_read, mut session_write) = tokio::io::split(relay);
                let inbound = async move {
                    let _ = tokio::io::copy(&mut client_read, &mut session_write).await;
                    // A helper that disappears without its explicit EOF frame must still
                    // close Git stdin. Keep the relay itself alive to drain the answer.
                    let _ = session_write.shutdown().await;
                    std::future::pending::<()>().await
                };
                timeout(protocol::SESSION_TIMEOUT, async {
                    let forwarded = tokio::select! {
                        result = tokio::io::copy(&mut session_read, &mut client_write) => result,
                        _ = inbound => Ok(0),
                    };
                    // Closing the relay makes Git's writes fail instead of blocking on a pipe
                    // nobody reads. The connection stays open until the session ends, so its
                    // hooks can still reach the broker while it drains.
                    drop(session_read);
                    let session = task.await.context("Git session task failed");
                    forwarded?;
                    session
                })
                .await??;
            }
            Request::Keys {
                user,
                key_type,
                base64_key,
                serve_command,
            } => {
                let _slot = timeout(protocol::CONTROL_TIMEOUT, self.controls.acquire())
                    .await?
                    .context("control requests busy")?;
                let line = timeout(
                    protocol::CONTROL_TIMEOUT,
                    self.keys(&user, &key_type, &base64_key, &serve_command),
                )
                .await??;
                if let Some(line) = line {
                    protocol::write_frame(stream, STDOUT, line.as_bytes()).await?;
                }
                protocol::finish(stream, 0).await?;
            }
            Request::Hook {
                key_id,
                repo_id,
                token,
                post,
                updates,
                quarantine,
            } => {
                let operation = self
                    .operations
                    .lock()
                    .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
                    .get(&token)
                    .cloned()
                    .context("no active receive operation")?;
                ensure!(
                    key_id == operation.credential.id() && repo_id == operation.repo_id,
                    "hook identity mismatch"
                );
                if post {
                    validation::validate_updates(&updates)?;
                    let mut state = operation
                        .state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("operation state unavailable"))?;
                    let proposed = state
                        .proposed
                        .as_ref()
                        .context("missing successful pre-receive hook")?;
                    ensure!(
                        !state.post_started
                            && updates.iter().all(|update| proposed.contains(update)),
                        "post-receive updates do not match admission"
                    );
                    state.post_started = true;
                    state.proposed = None;
                    state.dispatch_failed = true;
                    // Release before waiting for control capacity: other pre-hooks may
                    // occupy it while waiting for this publication's repository lock.
                    state.publication.take();
                }
                let _slot = timeout(protocol::CONTROL_TIMEOUT, self.controls.acquire())
                    .await?
                    .context("control requests busy")?;
                let result = timeout(
                    protocol::CONTROL_TIMEOUT,
                    self.hook(&operation, post, &updates, quarantine.as_deref()),
                )
                .await;
                if post {
                    operation
                        .state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
                        .dispatch_failed = !matches!(&result, Ok(Ok(())));
                }
                result??;
                protocol::finish(stream, 0).await?;
            }
        }
        Ok(())
    }

    async fn keys(
        &self,
        user: &str,
        key_type: &str,
        blob: &str,
        helper: &str,
    ) -> Result<Option<String>> {
        if user != self.git_user {
            return Ok(None);
        }
        let supplied = validation::canonical_key(key_type, blob)?;
        let fingerprint = db::users::ssh_keys::calculate_fingerprint(&supplied)?;
        let Some(identity) =
            db::users::ssh_keys::get_user_by_key_fingerprint(&self.pool, &fingerprint).await?
        else {
            return Ok(None);
        };
        if identity.user.is_banned || identity.user.must_change_password {
            return Ok(None);
        }
        let canonical =
            validation::canonical_db_key(&identity.key.public_key, &identity.key.key_type)?;
        ensure!(
            canonical == supplied
                && db::users::ssh_keys::calculate_fingerprint(&canonical)?
                    == identity.key.fingerprint
                && identity.key.id > 0,
            "stored key mismatch"
        );
        // This installer-owned wrapper receives the key ID as its sole argument;
        // it embeds the socket and calls this executable's `ssh serve` command.
        private_files::trusted_executable(Path::new(helper), self.uid)?;
        Ok(Some(format!(
            "restrict,command=\"{helper} {}\" {canonical}\n",
            identity.key.id
        )))
    }

    /// Where `repo` really is on disk, if that is a bare repository inside the
    /// repositories root. Git sessions run against this path.
    pub(super) async fn bare_path(&self, repo: &Repository) -> Result<PathBuf> {
        let root = self.repos_root.clone();
        let path = repo_store(repo, self.ci.cluster.as_ref())
            .local_path_for_spawning_git()?
            .to_owned();
        tokio::task::spawn_blocking(move || {
            let path = std::fs::canonicalize(path)?;
            ensure!(
                path.starts_with(&root) && path != root && path.is_dir(),
                "repository outside configured root"
            );
            let bare = git2::Repository::open_bare(&path)?;
            ensure!(
                bare.is_bare() && std::fs::canonicalize(bare.path())? == path,
                "real bare repository required"
            );
            Ok(path)
        })
        .await?
    }

    async fn serve(
        self: &Arc<Self>,
        stream: &mut UnixStream,
        key_id: i64,
        raw_command: &str,
        git_protocol: Option<&str>,
        slot: OwnedSemaphorePermit,
    ) -> Result<()> {
        let command = validation::parse_command(raw_command)?;
        let git_protocol = validation::protocol(git_protocol)?;
        let repo =
            db::repositories::records::get_repository(&self.pool, &command.owner, &command.repo)
                .await?
                .context("repository unavailable")?;
        ReceiveCredential::SshKey(key_id)
            .authorize(&self.pool, &repo, command.write)
            .await?;
        let path = self.bare_path(&repo).await?;
        let _slot = slot;
        let (input, mut child_input) = tokio::io::duplex(CHUNK);
        let (mut child_output, output) = tokio::io::duplex(CHUNK);
        let (mut child_errors, errors) = tokio::io::duplex(CHUNK);
        let (reader, mut writer) = stream.split();
        let inbound = framed_input(reader, input);
        let outbound = framed_output(output, errors, &mut writer);
        let options = GitSession {
            service: if command.write {
                GitService::ReceivePack
            } else {
                GitService::UploadPack
            },
            stateless: false,
            git_protocol: git_protocol.map(str::to_owned),
            credential: Some(ReceiveCredential::SshKey(key_id)),
            owner: command.owner,
            allow_unreachable: false,
        };
        let session = async move {
            self.session(
                &repo,
                path,
                options,
                GitStreams {
                    input: &mut child_input,
                    output: &mut child_output,
                    errors: &mut child_errors,
                },
            )
            .await
        };
        // A lost relay closes stdin but does not cancel an accepted push.
        let status = {
            tokio::pin!(session, inbound, outbound);
            tokio::select! {
                result = &mut session => { let _ = outbound.await; result? }
                _ = &mut inbound => {
                    let (result, _) = tokio::join!(session, outbound);
                    result?
                }
                _ = &mut outbound => session.await?,
            }
        };
        protocol::finish(&mut writer, status.code().unwrap_or(1).clamp(0, 255)).await
    }

    async fn session(
        self: &Arc<Self>,
        repo: &Repository,
        path: PathBuf,
        options: GitSession,
        streams: GitStreams<'_>,
    ) -> Result<std::process::ExitStatus> {
        let write = options.service == GitService::ReceivePack;
        let credential = options.credential.context("Git credential required")?;
        credential.authorize(&self.pool, repo, write).await?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let hooks = self.make_hooks(credential.id(), repo.id, write)?;
        let operation = Arc::new(Operation {
            credential,
            repo_id: repo.id,
            owner: options.owner,
            path,
            hooks,
            state: Mutex::new(HookState::default()),
        });
        let _active = if write {
            self.operations
                .lock()
                .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
                .insert(token.clone(), operation.clone());
            Some(ActiveOperation {
                gateway: self.clone(),
                token: token.clone(),
            })
        } else {
            None
        };
        // A push can move some refs before it fails, so every receive is counted as a change
        // before receive-pack starts. All transports, host OpenSSH included, end up here.
        let _change = if write {
            Some(
                begin_ref_change(
                    &self.pool,
                    repo.id,
                    &operation.path,
                    Some(repo.storage_node_id),
                )
                .await?,
            )
        } else {
            None
        };
        let mut command_process = self.git_command(&operation, &token, write, options.stateless)?;
        if let Some(protocol) = validation::protocol(options.git_protocol.as_deref())? {
            command_process.env("GIT_PROTOCOL", protocol);
        }
        let status = git_process::stream(
            command_process,
            streams.input,
            streams.output,
            &mut *streams.errors,
            write,
        )
        .await?;
        if operation
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
            .dispatch_failed
        {
            let _ = streams
                .errors
                .write_all(
                    b"Git writes were accepted; post-push dispatch was not fully successful.\n",
                )
                .await;
            return Err(super::PostReceiveFailed.into());
        }
        Ok(status)
    }

    fn git_command(
        &self,
        operation: &Operation,
        token: &str,
        write: bool,
        stateless: bool,
    ) -> Result<tokio::process::Command> {
        let mut command = git_process::command(&self.git)?;
        command
            .env("HOME", operation.hooks.path())
            .env("GITSERV_OPENSSH_OPERATION", token)
            .arg("-c")
            .arg(format!(
                "core.hooksPath={}",
                validation::shell_safe_path(operation.hooks.path())?
            ))
            .arg(if write { "receive-pack" } else { "upload-pack" });
        if stateless {
            command.arg("--stateless-rpc");
        }
        command
            .arg(&operation.path)
            .current_dir(operation.hooks.path());
        Ok(command)
    }

    fn make_hooks(&self, key_id: i64, repo_id: i64, write: bool) -> Result<TempDir> {
        let directory = tempfile::Builder::new()
            .prefix("receive-hooks-")
            .tempdir_in(self.socket.parent().context("socket parent required")?)?;
        validation::shell_safe_path(directory.path())?;
        if write {
            for name in ["pre-receive", "post-receive"] {
                let path = directory.path().join(name);
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o700)
                    .open(&path)?;
                writeln!(
                    file,
                    "#!/bin/sh\nexec {} ssh {} --socket {} --key-id {} --repo-id {}",
                    validation::shell_safe_path(&self.executable)?,
                    name,
                    validation::shell_safe_path(&self.socket)?,
                    key_id,
                    repo_id
                )?;
                file.sync_all()?;
            }
        }
        Ok(directory)
    }
}

async fn framed_input<R: AsyncRead + Unpin>(
    mut reader: R,
    mut input: impl AsyncWrite + Unpin,
) -> Result<()> {
    loop {
        let (tag, bytes) = protocol::read_frame(&mut reader).await?;
        match tag {
            STDIN => input.write_all(&bytes).await?,
            EOF => {
                input.shutdown().await?;
                return Ok(());
            }
            _ => anyhow::bail!("unexpected client frame"),
        }
    }
}

async fn framed_output<R: AsyncRead + Unpin, E: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut output: R,
    mut errors: E,
    writer: &mut W,
) -> Result<()> {
    let mut out = vec![0; CHUNK];
    let mut err = vec![0; CHUNK];
    let mut out_open = true;
    let mut err_open = true;
    while out_open || err_open {
        tokio::select! {
            count = output.read(&mut out), if out_open => {
                let count = count?;
                if count == 0 { out_open = false; } else { protocol::write_frame(writer, STDOUT, &out[..count]).await?; }
            }
            count = errors.read(&mut err), if err_open => {
                let count = count?;
                if count == 0 { err_open = false; } else { protocol::write_frame(writer, STDERR, &err[..count]).await?; }
            }
        }
    }
    Ok(())
}

// Keep disposable SQL fixture setup outside production backend source. This
// remains a private unit-test module, never an application DB access path.
#[cfg(test)]
#[path = "policy_tests.rs"]
pub(super) mod policy_tests;
#[cfg(test)]
#[path = "../../../../tests/support/openssh_gateway.rs"]
mod tests;

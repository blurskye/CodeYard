//! Running `git`: scrubbed environment, hooks off, bounded output and timeouts.
use anyhow::{bail, ensure, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

pub(crate) const SESSION_TIMEOUT: Duration = Duration::from_secs(3600);
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub(crate) const CHUNK: usize = 32 * 1024;
/// How long a Git session waits for a free slot before the request fails as busy:
/// `GIT_SESSION_WAIT_SECONDS`, default 30.
static SESSION_WAIT: LazyLock<Duration> = LazyLock::new(|| {
    let seconds = std::env::var("GIT_SESSION_WAIT_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    Duration::from_secs(seconds.unwrap_or(30))
});
static SESSION_CAPACITY: LazyLock<usize> = LazyLock::new(session_limit);
static RPC_SESSIONS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(session_capacity())));
static GIT_OPERATIONS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(16)));

/// Git sessions at once on this node: `GIT_SESSION_LIMIT`, or four per CPU core and at least 16.
fn session_limit() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |cores| cores.get());
    std::env::var("GIT_SESSION_LIMIT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or((cores * 4).max(16))
}

pub(crate) fn session_capacity() -> usize {
    *SESSION_CAPACITY
}

pub(crate) fn session_wait() -> Duration {
    *SESSION_WAIT
}

pub(crate) fn session_slots() -> Arc<Semaphore> {
    RPC_SESSIONS.clone()
}

#[derive(Debug, thiserror::Error)]
#[error("Git sessions busy")]
pub(crate) struct SessionsBusy;

/// A Git session slot. A busy node makes the request wait for a short time, as a queue would,
/// instead of failing every push or clone above the limit.
pub(crate) async fn session_permit() -> Result<OwnedSemaphorePermit> {
    wait_for_slot(RPC_SESSIONS.clone(), *SESSION_WAIT).await
}

pub(crate) async fn wait_for_slot(
    slots: Arc<Semaphore>,
    wait: Duration,
) -> Result<OwnedSemaphorePermit> {
    match tokio::time::timeout(wait, slots.acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        _ => Err(SessionsBusy.into()),
    }
}

pub(crate) const RPC_LIMIT: usize = 128 * 1024 * 1024;
pub(crate) const RPC_TIMEOUT: Duration = Duration::from_secs(300);
const STDERR_LIMIT: usize = 1024 * 1024;

/// Absolute path of `git` from the server's PATH, resolved once. Children get a
/// cleared environment, so they can't look it up themselves.
pub(crate) fn executable() -> Result<PathBuf> {
    static GIT: OnceLock<Option<PathBuf>> = OnceLock::new();
    GIT.get_or_init(|| {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .filter(|directory| directory.is_absolute())
            .map(|directory| directory.join("git"))
            .find(|path| path.is_file())
    })
    .clone()
    .context("git executable not found on PATH")
}

/// A `git` command that ignores user/system config and repository hooks.
pub(crate) fn std_command(executable: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(executable);
    command
        .env_clear()
        .env("PATH", executable.parent().unwrap_or(Path::new("/usr/bin")))
        .env("HOME", "/nonexistent")
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "receive.autogc=false",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

pub(crate) fn command(executable: &Path) -> Result<Command> {
    let mut command = Command::from(std_command(executable));
    // Own process group so a timeout can kill git and anything it started.
    command.process_group(0).kill_on_drop(true);
    Ok(command)
}

pub(crate) fn git_command() -> Result<Command> {
    command(&executable()?)
}

#[derive(Debug, thiserror::Error)]
#[error("git output exceeded its limit")]
pub(crate) struct OutputLimitExceeded;

pub(crate) async fn read_bounded(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > limit {
        return Err(OutputLimitExceeded.into());
    }
    Ok(bytes)
}

/// Kills the whole process group of a child started by [`command`].
pub(crate) fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: plain syscall; a negative pid addresses the child's own group.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

/// Keep only the last diagnostics when a transport reports stderr at exit.
#[derive(Default)]
pub(crate) struct StderrTail(Vec<u8>);

impl StderrTail {
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}

impl AsyncWrite for StderrTail {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        const LIMIT: usize = 16 * 1024;
        let tail = &bytes[bytes.len().saturating_sub(LIMIT)..];
        let remove = (self.0.len() + tail.len()).saturating_sub(LIMIT);
        self.0.drain(..remove);
        self.0.extend_from_slice(tail);
        std::task::Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

pub(crate) struct ProcessGroup(pub(crate) Option<u32>);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        kill_group(self.0);
    }
}

/// Copies with fixed memory, reporting traffic in either direction to the idle timer.
pub(crate) async fn copy(
    mut reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    activity: watch::Sender<Instant>,
    drain_on_disconnect: bool,
) -> Result<()> {
    let mut buffer = vec![0; CHUNK];
    let mut disconnected = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        activity.send_replace(Instant::now());
        if !disconnected {
            // A TLS writer can accept a chunk while its records wait behind a full socket. Git
            // waits for its peer to answer each part of the conversation, so every chunk is
            // pushed out before the next one is read.
            let sent = match writer.write_all(&buffer[..count]).await {
                Ok(()) => writer.flush().await,
                Err(error) => Err(error),
            };
            match sent {
                Ok(()) => {
                    activity.send_replace(Instant::now());
                }
                Err(_) if drain_on_disconnect => disconnected = true,
                Err(error) => return Err(error.into()),
            }
        }
    }
}

pub(crate) async fn session_timeout<T>(
    work: impl std::future::Future<Output = Result<T>>,
    mut activity: watch::Receiver<Instant>,
    idle: Duration,
) -> Result<T> {
    let idle_timer = async {
        loop {
            let deadline = *activity.borrow_and_update() + idle;
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => bail!("Git session idle timeout"),
                result = activity.changed() => { result.context("Git session activity closed")?; }
            }
        }
    };
    tokio::time::timeout(SESSION_TIMEOUT, async {
        tokio::select! {
            result = work => result,
            result = idle_timer => result,
        }
    })
    .await
    .context("Git session timed out")?
}

/// Runs a live Git process. Closing stdin delivers EOF; Git, not the transport,
/// decides whether a complete pack arrived. Receive output is drained after a
/// disconnect so already accepted writes can finish their hooks.
pub(crate) async fn stream(
    command: Command,
    input: impl AsyncRead + Unpin,
    output: impl AsyncWrite + Unpin,
    errors: impl AsyncWrite + Unpin,
    receive: bool,
) -> Result<ExitStatus> {
    stream_with_idle(command, input, output, errors, receive, IDLE_TIMEOUT).await
}

async fn stream_with_idle(
    mut command: Command,
    input: impl AsyncRead + Unpin,
    output: impl AsyncWrite + Unpin,
    errors: impl AsyncWrite + Unpin,
    receive: bool,
    idle: Duration,
) -> Result<ExitStatus> {
    let mut child = command.spawn().context("start git")?;
    let mut group = ProcessGroup(child.id());
    let stdin = child.stdin.take().context("git stdin")?;
    let stdout = child.stdout.take().context("git stdout")?;
    let stderr = child.stderr.take().context("git stderr")?;
    let (activity, receiver) = watch::channel(Instant::now());
    let work = async {
        let inbound = copy(input, stdin, activity.clone(), false);
        let outbound = async {
            tokio::try_join!(
                copy(stdout, output, activity.clone(), receive),
                copy(stderr, errors, activity.clone(), receive),
            )?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::pin!(inbound, outbound);
        let wait = child.wait();
        tokio::pin!(wait);
        let (status, drained) = tokio::select! {
            status = &mut wait => (status?, false),
            result = &mut inbound => {
                // Git can close stdin after a rejection or a complete pack. A
                // lost receive input becomes EOF, letting Git reject short packs.
                if let Err(error) = result {
                    if !receive && !error.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) {
                        return Err(error);
                    }
                }
                tokio::select! {
                    status = &mut wait => (status?, false),
                    result = &mut outbound => { result?; (wait.await?, true) }
                }
            }
            result = &mut outbound => { result?; (wait.await?, true) }
        };
        if !drained {
            outbound.await?;
        }
        Ok(status)
    };
    let result = session_timeout(work, receiver, idle).await;
    if result.is_ok() {
        group.0 = None;
    }
    result
}

pub(crate) async fn operation_slot(timeout: Duration) -> Result<OwnedSemaphorePermit> {
    tokio::time::timeout(timeout, GIT_OPERATIONS.clone().acquire_owned())
        .await
        .context("git timed out")?
        .context("Git operation limit closed")
}

/// Runs a command to completion, feeding `input` and collecting bounded output.
pub(crate) async fn run(
    mut command: Command,
    input: &[u8],
    output_limit: usize,
    timeout: Duration,
) -> Result<Output> {
    ensure!(input.len() <= RPC_LIMIT, "git request exceeded its limit");
    let started = Instant::now();
    let _permit = operation_slot(timeout).await?;
    let remaining = timeout.saturating_sub(started.elapsed());
    let mut child = command.spawn().context("start git")?;
    let mut group = ProcessGroup(child.id());
    let mut stdin = child.stdin.take().context("git stdin")?;
    let stdout = child.stdout.take().context("git stdout")?;
    let stderr = child.stderr.take().context("git stderr")?;
    let work = async {
        let write = async move {
            stdin.write_all(input).await?;
            drop(stdin);
            Ok::<_, anyhow::Error>(())
        };
        let (_, stdout, stderr) = tokio::try_join!(
            write,
            read_bounded(stdout, output_limit),
            read_bounded(stderr, STDERR_LIMIT),
        )?;
        let status = child.wait().await?;
        Ok::<_, anyhow::Error>(Output {
            status,
            stdout,
            stderr,
        })
    };
    let result = tokio::time::timeout(remaining, work)
        .await
        .context("git timed out")?;
    if result.is_ok() {
        group.0 = None;
    }
    result
}

pub(crate) async fn buffered(
    command: Command,
    input: &[u8],
    output_limit: usize,
    timeout: Duration,
) -> Result<(ExitStatus, Vec<u8>)> {
    let output = run(command, input, output_limit, timeout).await?;
    Ok((output.status, output.stdout))
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;

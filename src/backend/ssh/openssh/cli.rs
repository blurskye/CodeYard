use super::Commands;
use crate::backend::repositories::git_sessions::private_files::connect as connect_private;
use crate::backend::repositories::git_sessions::protocol::{
    self, Request, CHUNK, EOF, EXIT, STDERR, STDIN, STDOUT,
};
use crate::backend::repositories::git_sessions::validation::{
    canonical_key, parse_command, parse_updates, protocol as git_protocol, shell_safe_path,
    MAX_HOOK_BYTES,
};
use anyhow::{ensure, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use tokio::io::{unix::AsyncFd, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::time::{timeout, Duration};

pub(super) async fn run(command: Commands) -> Result<i32> {
    let (socket, request, streaming) = match command {
        Commands::Keys {
            socket,
            expected_user,
            serve_command,
            user,
            key_type,
            base64_key,
        } => {
            shell_safe_path(&socket)?;
            let helper = shell_safe_path(&serve_command)?.to_owned();
            if user != expected_user {
                return Ok(0);
            }
            canonical_key(&key_type, &base64_key)?;
            (
                socket,
                Request::Keys {
                    user,
                    key_type,
                    base64_key,
                    serve_command: helper,
                },
                false,
            )
        }
        Commands::Serve {
            socket,
            key_id,
            deploy_key_id,
        } => {
            use crate::backend::repositories::git_sessions::ReceiveCredential;
            let credential = match (key_id, deploy_key_id) {
                (Some(id), None) => ReceiveCredential::SshKey(id),
                (None, Some(id)) => ReceiveCredential::DeployKey(id),
                _ => anyhow::bail!("one SSH or deploy key is required"),
            };
            let command = std::env::var("SSH_ORIGINAL_COMMAND").map_err(|_| {
                anyhow::anyhow!("SSH_ORIGINAL_COMMAND is required; interactive shells are disabled")
            })?;
            parse_command(&command)?;
            let protocol = match std::env::var("GIT_PROTOCOL") {
                Ok(value) => git_protocol(Some(&value))?.map(str::to_owned),
                Err(std::env::VarError::NotPresent) => None,
                Err(_) => anyhow::bail!("invalid GIT_PROTOCOL"),
            };
            (
                socket,
                Request::Serve {
                    credential,
                    command,
                    git_protocol: protocol,
                },
                true,
            )
        }
        Commands::PreReceive {
            socket,
            key_id,
            repo_id,
        } => {
            let request = hook_request(key_id, repo_id, false).await?;
            refuse_unreadable_objects().await?;
            (socket, request, false)
        }
        Commands::PostReceive {
            socket,
            key_id,
            repo_id,
        } => (socket, hook_request(key_id, repo_id, true).await?, false),
    };
    let mut connection = connect_private(&socket).await?;
    protocol::write_request(&mut connection, &request).await?;
    let (mut reader, mut writer) = connection.into_split();
    let mut output = stdio(std::io::stdout().as_fd())?;
    let mut errors = stdio(std::io::stderr().as_fd())?;
    let receive = receive_output(&mut reader, &mut output, &mut errors);
    tokio::pin!(receive);
    let status = if streaming {
        let input = pump_input(stdin()?, &mut writer);
        tokio::pin!(input);
        timeout(protocol::SESSION_TIMEOUT, async {
            tokio::select! {
                result = &mut receive => result,
                result = &mut input => {
                    // A broker that has finished the session (a refusal, say) closes the
                    // socket while we may still be sending. Its last frames, the message and
                    // the exit status, are then waiting to be read.
                    if let Err(error) = result {
                        let closed = error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                            matches!(
                                error.kind(),
                                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                            )
                        });
                        if !closed {
                            return Err(error);
                        }
                    }
                    receive.await
                }
            }
        })
        .await??
    } else {
        timeout(protocol::CONTROL_TIMEOUT, receive).await??
    };
    Ok(status)
}

/// Refuses a push that carries a commit or tag object larger than the forge ever reads
/// back. Git keeps the objects of a push in a quarantine until pre-receive accepts it, so
/// the quarantine holds exactly the new objects and listing their sizes reads no content.
async fn refuse_unreadable_objects() -> Result<()> {
    use crate::backend::git::repository::{process, MAX_COMMIT_OBJECT_BYTES};
    use tokio::io::{AsyncBufReadExt, BufReader};

    let Some(quarantine) = std::env::var_os("GIT_QUARANTINE_PATH") else {
        return Ok(());
    };
    let mut command = process::command(&process::executable()?)?;
    command
        .env("GIT_DIR", std::env::var_os("GIT_DIR").unwrap_or(".".into()))
        .env("GIT_OBJECT_DIRECTORY", quarantine)
        .args([
            "cat-file",
            "--batch-all-objects",
            "--unordered",
            "--batch-check=%(objecttype) %(objectsize)",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn()?;
    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    while let Some(line) = lines.next_line().await? {
        let size = match line.split_once(' ') {
            Some(("commit" | "tag", size)) => size.parse::<usize>().unwrap_or(usize::MAX),
            _ => continue,
        };
        ensure!(
            size <= MAX_COMMIT_OBJECT_BYTES,
            "this push contains a commit or tag larger than {} KiB",
            MAX_COMMIT_OBJECT_BYTES / 1024
        );
    }
    ensure!(
        child.wait().await?.success(),
        "could not list pushed objects"
    );
    Ok(())
}

async fn hook_request(key_id: i64, repo_id: i64, post: bool) -> Result<Request> {
    ensure!(key_id > 0 && repo_id > 0, "invalid key/repository ID");
    let token = std::env::var("GITSERV_OPENSSH_OPERATION")
        .map_err(|_| anyhow::anyhow!("receive hooks must belong to an active broker operation"))?;
    ensure!(
        crate::backend::crypto::is_hex_token(&token, 32),
        "invalid operation token"
    );
    let mut bytes = Vec::new();
    timeout(
        Duration::from_secs(15),
        stdin()?
            .take((MAX_HOOK_BYTES + 1) as u64)
            .read_to_end(&mut bytes),
    )
    .await??;
    let updates = parse_updates(&bytes)?;
    Ok(Request::Hook {
        key_id,
        repo_id,
        token,
        post,
        updates,
        quarantine: (!post)
            .then(|| std::env::var_os("GIT_QUARANTINE_PATH"))
            .flatten()
            .and_then(|path| std::path::absolute(path).ok())
            .and_then(|path| path.into_os_string().into_string().ok()),
    })
}

async fn pump_input<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut input: R,
    writer: &mut W,
) -> Result<()> {
    let mut bytes = [0; CHUNK];
    loop {
        let count = input.read(&mut bytes).await?;
        if count == 0 {
            protocol::write_frame(writer, EOF, &[]).await?;
            // Do not half-close: keep the socket open until the exit frame so
            // the broker can distinguish stdin EOF from a helper disconnect.
            return Ok(());
        }
        protocol::write_frame(writer, STDIN, &bytes[..count]).await?;
    }
}

async fn receive_output<R: AsyncRead + Unpin, W: AsyncWrite + Unpin, E: AsyncWrite + Unpin>(
    reader: &mut R,
    output: &mut W,
    errors: &mut E,
) -> Result<i32> {
    loop {
        let (tag, bytes) = protocol::read_frame(reader).await?;
        match tag {
            STDOUT => {
                output.write_all(&bytes).await?;
                output.flush().await?;
            }
            STDERR => {
                errors.write_all(&bytes).await?;
                errors.flush().await?;
            }
            EXIT => {
                let status = i32::from_be_bytes(bytes.as_slice().try_into()?);
                ensure!((0..=255).contains(&status), "invalid broker exit status");
                return Ok(status);
            }
            _ => anyhow::bail!("unexpected broker frame"),
        }
    }
}

// Do not use Tokio's permanently blocking stdin worker: it can prevent helper
// exit while remote stdin stays open. Dup + readiness also supports sshd's Unix
// socketpair stdio, which cannot be reopened through /proc. All three standard
// streams use readiness because socketpair stdin/stdout may share status flags.
enum StandardIo {
    Regular(tokio::fs::File),
    Pollable(AsyncFd<File>),
}

fn stdin() -> Result<StandardIo> {
    stdio(std::io::stdin().as_fd())
}

fn stdio(fd: BorrowedFd<'_>) -> Result<StandardIo> {
    let file = File::from(fd.try_clone_to_owned()?);
    let metadata = file.metadata()?;
    let null = metadata.file_type().is_char_device()
        && metadata.rdev() == std::fs::metadata("/dev/null")?.rdev();
    if metadata.is_file() || null {
        return Ok(StandardIo::Regular(tokio::fs::File::from_std(file)));
    }
    // Prefer a separate open description for pipes/terminals: changing flags
    // on a dup could otherwise affect inherited Git hook stderr in the parent.
    // Socketpairs cannot be reopened and intentionally share helper stdio flags.
    // SAFETY: the owned fd is live; F_GETFL/F_SETFL use scalar arguments.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let reopened = OpenOptions::new()
        .read(flags & libc::O_ACCMODE != libc::O_WRONLY)
        .write(flags & libc::O_ACCMODE != libc::O_RDONLY)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(format!("/proc/self/fd/{}", file.as_raw_fd()));
    let file = match reopened {
        Ok(file) => file,
        Err(_) => {
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            file
        }
    };
    Ok(StandardIo::Pollable(AsyncFd::new(file)?))
}

impl AsyncWrite for StandardIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Regular(file) => Pin::new(file).poll_write(cx, bytes),
            Self::Pollable(file) => loop {
                let mut guard = ready!(file.poll_write_ready(cx))?;
                match guard.try_io(|inner| {
                    let mut file = inner.get_ref();
                    file.write(bytes)
                }) {
                    Ok(result) => return Poll::Ready(result),
                    Err(_) => continue,
                }
            },
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Regular(file) => Pin::new(file).poll_flush(cx),
            Self::Pollable(_) => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl AsyncRead for StandardIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Regular(file) => Pin::new(file).poll_read(cx, buffer),
            Self::Pollable(file) => loop {
                let mut guard = ready!(file.poll_read_ready(cx))?;
                match guard.try_io(|inner| {
                    let mut file = inner.get_ref();
                    file.read(buffer.initialize_unfilled())
                }) {
                    Ok(Ok(count)) => {
                        buffer.advance(count);
                        return Poll::Ready(Ok(()));
                    }
                    Ok(Err(error)) => return Poll::Ready(Err(error)),
                    Err(_) => continue,
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn helper_stdio_handles_sshd_socketpairs_without_blocking_worker_threads() {
        let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut input = stdio(left.as_fd()).unwrap();
        let mut output = stdio(left.as_fd()).unwrap();
        right.set_nonblocking(true).unwrap();
        let mut peer = tokio::net::UnixStream::from_std(right).unwrap();
        peer.write_all(b"request").await.unwrap();
        let mut bytes = [0; 7];
        input.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"request");
        output.write_all(b"reply").await.unwrap();
        let mut response = [0; 5];
        peer.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"reply");
    }
}

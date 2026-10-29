//! Child process lifetime following actions/runner's `Runner.Sdk/ProcessInvoker.cs`.
use super::commands::{Commands, Log};
use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

/// Longest line kept from a child process. A longer line is drained and replaced by a notice,
/// so a child that writes without newlines cannot grow the runner's memory. The line is dropped
/// rather than cut, because a cut can keep the start of a secret that masking no longer matches.
const MAX_LINE: usize = 64 * 1024;

pub(crate) async fn read_lines(reader: impl AsyncRead + Unpin, sender: mpsc::Sender<String>) {
    let mut reader = BufReader::new(reader);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read = (&mut reader)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut buffer)
            .await;
        if matches!(read, Ok(0) | Err(_)) {
            break;
        }
        let line = if buffer.len() > MAX_LINE && !buffer.ends_with(b"\n") {
            if skip_line(&mut reader).await.is_err() {
                break;
            }
            format!(
                "Warning: An output line longer than {} KiB was omitted.",
                MAX_LINE / 1024
            )
        } else {
            String::from_utf8_lossy(&buffer)
                .trim_end_matches(['\n', '\r'])
                .to_owned()
        };
        if sender.send(line).await.is_err() {
            break;
        }
    }
}

/// Discards the rest of a line in bounded pieces, so the child keeps running.
async fn skip_line<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> std::io::Result<()> {
    let mut piece = Vec::new();
    loop {
        piece.clear();
        let read = (&mut *reader)
            .take(MAX_LINE as u64)
            .read_until(b'\n', &mut piece)
            .await?;
        if read == 0 || piece.ends_with(b"\n") {
            return Ok(());
        }
    }
}

struct ProcessGroup(Option<u32>);
impl ProcessGroup {
    fn terminate(&mut self) {
        if let Some(id) = self.0.take() {
            #[cfg(unix)]
            // Each child starts a new process group, so grandchildren cannot survive cancellation.
            unsafe {
                libc::kill(-(id as i32), libc::SIGKILL);
            }
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

pub(super) async fn output(
    command: &mut Command,
    timeout: Duration,
) -> Result<std::process::Output> {
    command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("Unable to start process")?;
    let mut group = ProcessGroup(child.id());
    let stdout = child.stdout.take().context("No stdout")?;
    let stderr = child.stderr.take().context("No stderr")?;
    async fn capture(reader: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        reader.take(1024 * 1024 + 1).read_to_end(&mut bytes).await?;
        anyhow::ensure!(bytes.len() <= 1024 * 1024, "Process output exceeds the capture limit");
        Ok(bytes)
    }
    let result = tokio::time::timeout(timeout, async {
        let (status, stdout, stderr) = tokio::try_join!(
            async { child.wait().await.map_err(anyhow::Error::from) },
            capture(stdout), capture(stderr),
        )?;
        Ok::<_, anyhow::Error>(std::process::Output { status, stdout, stderr })
    }).await.context("The operation has timed out.").and_then(|result| result);
    group.terminate();
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result
}

pub(super) async fn execute(
    command: &mut Command,
    log: &Log,
    commands: &mut Commands,
    cancel: &mut watch::Receiver<bool>,
    deadline: Instant,
    allow_unsafe: bool,
) -> Result<bool> {
    if *cancel.borrow() || Instant::now() >= deadline {
        return Ok(false);
    }
    command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("Unable to start process")?;
    let mut group = ProcessGroup(Some(child.id().context("Process has no id")?));
    let (tx, mut rx) = mpsc::channel(128);
    let stdout = tokio::spawn(read_lines(
        child.stdout.take().context("No stdout")?,
        tx.clone(),
    ));
    let stderr = tokio::spawn(read_lines(child.stderr.take().context("No stderr")?, tx));
    let end = tokio::time::sleep_until(deadline.into());
    tokio::pin!(end);
    let mut status = None;
    let mut closed = false;
    loop {
        tokio::select! {
            biased;
            _ = cancel.changed() => {
                if *cancel.borrow() {
                    log.line("Cancelled: stopping child processes");
                    let _ = child.kill().await;
                    stdout.abort(); stderr.abort();
                    return Ok(false);
                }
            }
            _ = &mut end => {
                log.line("Error: The operation has timed out.");
                let _ = child.kill().await;
                stdout.abort(); stderr.abort();
                return Ok(false);
            }
            result = child.wait(), if status.is_none() => { status = Some(result?); group.terminate(); }
            line = rx.recv(), if !closed => {
                if let Some(line) = line { commands.line(log, &line, allow_unsafe); }
                else { closed = true; }
            }
        }
        if closed {
            if let Some(status) = status {
                return Ok(status.success() && !commands.failed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn over_long_lines_are_dropped_and_the_rest_is_drained() {
        let mut input = vec![b'x'; MAX_LINE * 3];
        input.extend_from_slice(b"\nnext\n");
        input.extend(vec![b'y'; MAX_LINE]);
        input.push(b'\n');
        let (sender, mut lines) = mpsc::channel(8);
        read_lines(&input[..], sender).await;
        let notice = lines.recv().await.unwrap();
        assert!(notice.starts_with("Warning: An output line longer than"));
        assert!(!notice.contains('x'));
        assert_eq!(lines.recv().await.unwrap(), "next");
        assert_eq!(lines.recv().await.unwrap().len(), MAX_LINE);
        assert!(lines.recv().await.is_none());
    }
}

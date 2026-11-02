//! Child process lifetime following actions/runner's `Runner.Sdk/ProcessInvoker.cs`.
use super::commands::{Commands, Log};
use crate::process::ProcessGroup;
use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

/// Longest line kept from a child process. A longer line is drained and replaced by a notice,
/// so a child that writes without newlines cannot grow the runner's memory. The line is dropped
/// rather than cut, because a cut can keep the start of a secret that masking no longer matches.
const MAX_LINE: usize = 64 * 1024;

/// How long output is read after a contained step's command exits. A background process the
/// step left running may keep the output pipes open; the step does not wait for it forever.
const OUTPUT_DRAIN: Duration = Duration::from_secs(3);

pub(crate) async fn read_lines(
    reader: impl AsyncRead + Unpin,
    sender: mpsc::Sender<Result<(String, bool)>>,
    stderr: bool,
) {
    let mut reader = BufReader::new(reader);
    loop {
        match crate::process::read_line(&mut reader, MAX_LINE, None).await {
            Ok(Some(line)) => {
                if sender.send(Ok((line, stderr))).await.is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(error) => {
                let _ = sender.send(Err(error.into())).await;
                return;
            }
        }
    }
}

/// The most output kept from a process the runner reads itself, such as Git or Podman.
const OUTPUT_LIMIT: usize = 1024 * 1024;

pub(crate) async fn output(
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
    let mut group = ProcessGroup::new(child.id(), Duration::ZERO);
    let stdout = child.stdout.take().context("No stdout")?;
    let stderr = child.stderr.take().context("No stderr")?;
    /// Reads a stream to its end and keeps its first MiB. The rest is dropped, so a process
    /// cannot grow the runner's memory and a long image build log does not fail the build.
    async fn capture(mut reader: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
        let mut kept = Vec::new();
        let mut piece = [0; 8192];
        loop {
            let read = reader.read(&mut piece).await?;
            if read == 0 {
                return Ok(kept);
            }
            let room = OUTPUT_LIMIT - kept.len();
            kept.extend_from_slice(&piece[..read.min(room)]);
        }
    }
    let result = tokio::time::timeout(timeout, async {
        let (status, stdout, stderr) = tokio::try_join!(
            async { child.wait().await.map_err(anyhow::Error::from) },
            capture(stdout),
            capture(stderr),
        )?;
        Ok::<_, anyhow::Error>(std::process::Output {
            status,
            stdout,
            stderr,
        })
    })
    .await
    .context("The operation has timed out.")
    .and_then(|result| result);
    group.kill();
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result
}

/// Runs one step command. With `contained`, the command runs in the job's own cgroup, which
/// ends every process it left behind when the job ends. Without it, the command's process
/// group is killed as soon as the command exits.
pub(super) async fn execute(
    command: &mut Command,
    log: &Log,
    commands: &mut Commands,
    cancel: &mut watch::Receiver<bool>,
    deadline: Instant,
    allow_unsafe: bool,
    contained: bool,
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
    let mut group = ProcessGroup::new(
        Some(child.id().context("Process has no id")?),
        Duration::ZERO,
    );
    let (tx, mut rx) = mpsc::channel(128);
    let stdout = tokio::spawn(read_lines(
        child.stdout.take().context("No stdout")?,
        tx.clone(),
        false,
    ));
    let stderr = tokio::spawn(read_lines(
        child.stderr.take().context("No stderr")?,
        tx,
        true,
    ));
    let end = tokio::time::sleep_until(deadline.into());
    tokio::pin!(end);
    let drain = tokio::time::sleep_until(deadline.into());
    tokio::pin!(drain);
    let mut draining = false;
    let mut status = None;
    let mut closed = false;
    loop {
        tokio::select! {
            biased;
            // A closed channel never cancels, rather than waking the loop again and again.
            _ = super::raised(cancel) => {
                log.line("Cancelled: stopping child processes");
                let _ = child.kill().await;
                stdout.abort(); stderr.abort();
                return Ok(false);
            }
            _ = &mut end => {
                log.line("Error: The operation has timed out.");
                let _ = child.kill().await;
                stdout.abort(); stderr.abort();
                return Ok(false);
            }
            result = child.wait(), if status.is_none() => {
                status = Some(result?);
                if contained {
                    // Background processes of the step stay alive for later steps.
                    group.disarm();
                    drain.as_mut().reset(tokio::time::Instant::now() + OUTPUT_DRAIN);
                    draining = true;
                } else {
                    group.kill();
                }
            }
            // Stopping the readers drops their senders, so the lines already queued are still
            // handled before the channel reports closed.
            _ = &mut drain, if draining && !closed => {
                draining = false;
                stdout.abort();
                stderr.abort();
            }
            line = rx.recv(), if !closed => {
                if let Some(line) = line {
                    let (line, stderr) = match line {
                        Ok(line) => line,
                        Err(error) => { stdout.abort(); stderr.abort(); return Err(error); }
                    };
                    commands.stream_line(log, &line, allow_unsafe, stderr);
                } else { closed = true; }
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

    #[cfg(unix)]
    #[tokio::test]
    async fn output_keeps_only_the_start_of_a_long_stream() {
        let mut command = Command::new("sh");
        command.args(["-c", "head -c 3000000 /dev/zero; echo done >&2"]);
        let output = output(&mut command, Duration::from_secs(30)).await.unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), OUTPUT_LIMIT);
        assert_eq!(output.stderr, b"done\n");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closed_cancel_channel_lets_the_process_finish() {
        let task = tokio::spawn(async {
            let temp = tempfile::tempdir().unwrap();
            let log = Log {
                prefix: "test".into(),
                ..Default::default()
            };
            let mut commands = Commands::new(temp.path().into(), Default::default(), false);
            let (sender, mut cancel) = watch::channel(false);
            drop(sender);
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 0.2; echo finished"]);
            execute(
                &mut command,
                &log,
                &mut commands,
                &mut cancel,
                Instant::now() + Duration::from_secs(30),
                false,
                false,
            )
            .await
            .unwrap()
        });
        let ok = tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .expect("the process finishes")
            .unwrap();
        assert!(ok);
    }

    #[tokio::test]
    async fn over_long_lines_are_dropped_and_the_rest_is_drained() {
        let mut input = vec![b'x'; MAX_LINE * 3];
        input.extend_from_slice(b"\nnext\n");
        input.extend(vec![b'y'; MAX_LINE]);
        input.push(b'\n');
        let (sender, mut lines) = mpsc::channel(8);
        read_lines(&input[..], sender, false).await;
        let (notice, _) = lines.recv().await.unwrap().unwrap();
        assert_eq!(notice, crate::process::OMITTED_LINE);
        assert!(!notice.contains('x'));
        assert_eq!(lines.recv().await.unwrap().unwrap().0, "next");
        assert_eq!(lines.recv().await.unwrap().unwrap().0.len(), MAX_LINE);
        assert!(lines.recv().await.is_none());
    }
}

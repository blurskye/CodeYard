//! Running CI commands: own process group, deadline, cancellation and
//! line-by-line output streaming.
use anyhow::{bail, Result};
use std::future::Future;
use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;

const MAX_LINE: usize = 16 * 1024;
const TRUNCATED_MARK: &str = " … [line truncated]";
const TERM_GRACE: Duration = Duration::from_secs(3);
const KILL_GRACE: Duration = Duration::from_secs(2);
const GROUP_POLL: Duration = Duration::from_millis(20);

/// A command with a minimal environment in its own process group.
pub(super) fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()),
        )
        .env("LANG", "C.UTF-8")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // A server started with SIGTERM ignored (nohup, some supervisors) would otherwise hand that
    // to every job, and cancelling a job could only end in SIGKILL.
    // SAFETY: `signal` is async-signal-safe, which is all `pre_exec` allows.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            Ok(())
        });
    }
    command
}

/// Why a command didn't finish normally.
#[derive(Debug, thiserror::Error)]
pub(super) enum Stopped {
    #[error("cancelled")]
    Cancelled,
    #[error("timed out")]
    TimedOut,
}

/// Runs `command`, passing each output line to `on_line(line, is_stderr)`.
/// Every return path terminates descendants left in the command's process group.
pub(super) async fn run<F, Fut>(
    mut command: Command,
    timeout: Duration,
    cancel: watch::Receiver<bool>,
    on_line: F,
) -> Result<ExitStatus>
where
    F: Fn(String, bool) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut child = command.spawn()?;
    let mut group = ProcessGroup::new(child.id());
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let work = async {
        let (_, _, status) = tokio::try_join!(
            forward(stdout, false, &on_line),
            forward(stderr, true, &on_line),
            async {
                let status = child.wait().await?;
                // Descendants can inherit the output pipes after the runner exits.
                stop_process_group(&mut child, group.pid()).await?;
                Ok::<_, anyhow::Error>(status)
            }
        )?;
        Ok::<_, anyhow::Error>(status)
    };
    let result = tokio::select! {
        result = tokio::time::timeout(timeout, work) => match result {
            Ok(result) => result,
            Err(_) => Err(Stopped::TimedOut.into()),
        },
        _ = cancelled(cancel) => Err(Stopped::Cancelled.into()),
    };
    stop_process_group(&mut child, group.pid()).await?;
    group.disarm();
    result
}

/// Resolves once `cancel` is set. If the sender is dropped without setting it, this
/// never resolves, so the other branch of the caller's select decides.
pub(super) async fn cancelled(mut cancel: watch::Receiver<bool>) {
    while !*cancel.borrow_and_update() {
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

struct ProcessGroup(Option<u32>);

impl ProcessGroup {
    fn new(pid: Option<u32>) -> Self {
        Self(pid)
    }

    fn pid(&self) -> Option<u32> {
        self.0
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let Some(pid) = self.0.take() else {
            return;
        };
        let _ = signal_group(pid, libc::SIGTERM);
        if std::thread::Builder::new()
            .name("ci-process-cleanup".into())
            .spawn(move || stop_process_group_blocking(pid))
            .is_err()
        {
            let _ = signal_group(pid, libc::SIGKILL);
        }
    }
}

async fn stop_process_group(child: &mut Child, pid: Option<u32>) -> Result<()> {
    let Some(pid) = pid else {
        return Ok(());
    };
    if !group_alive(pid)? {
        let _ = child.try_wait();
        return Ok(());
    }
    signal_group(pid, libc::SIGTERM)
        .map_err(|_| anyhow::anyhow!("could not stop CI process group"))?;
    if wait_for_group(child, pid, TERM_GRACE).await? {
        return Ok(());
    }
    signal_group(pid, libc::SIGKILL)
        .map_err(|_| anyhow::anyhow!("could not kill CI process group"))?;
    if wait_for_group(child, pid, KILL_GRACE).await? {
        return Ok(());
    }
    bail!("CI process group did not stop in time")
}

async fn wait_for_group(child: &mut Child, pid: u32, duration: Duration) -> Result<bool> {
    let deadline = Instant::now() + duration;
    loop {
        child
            .try_wait()
            .map_err(|_| anyhow::anyhow!("could not reap CI process"))?;
        if !group_alive(pid)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(GROUP_POLL).await;
    }
}

fn stop_process_group_blocking(pid: u32) {
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        if matches!(group_alive(pid), Ok(false)) {
            return;
        }
        std::thread::sleep(GROUP_POLL);
    }
    let _ = signal_group(pid, libc::SIGKILL);
}

fn group_alive(pid: u32) -> Result<bool> {
    let pid = i32::try_from(pid).map_err(|_| anyhow::anyhow!("invalid CI process group"))?;
    // SAFETY: signal zero only checks whether the process group still exists.
    if unsafe { libc::kill(-pid, 0) } == 0 {
        return Ok(true);
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => bail!("could not inspect CI process group"),
    }
}

fn signal_group(pid: u32, signal: i32) -> io::Result<()> {
    let pid = i32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid process group"))?;
    // SAFETY: a negative pid addresses the process group created for this command.
    if unsafe { libc::kill(-pid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

async fn forward<R, F, Fut>(reader: Option<R>, stderr: bool, on_line: &F) -> Result<()>
where
    R: AsyncRead + Unpin,
    F: Fn(String, bool) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let Some(reader) = reader else {
        return Ok(());
    };
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut reader)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
            .await?;
        if read == 0 {
            return Ok(());
        }
        // An over-long line has more input after this read, which is dropped below.
        let cut = line.len() > MAX_LINE && !line.ends_with(b"\n");
        if cut {
            skip_line(&mut reader).await?;
        }
        let mut text = String::from_utf8_lossy(&line)
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        // Lossy decoding can make the text longer than the bytes read, so the limit applies to
        // the text. The cut moves back to a character boundary, so no character is split.
        if cut || text.len() > MAX_LINE {
            let mut end = text.len().min(MAX_LINE - TRUNCATED_MARK.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str(TRUNCATED_MARK);
        }
        on_line(text, stderr).await?;
    }
}

/// Drops input through the next newline. It reads a bounded piece at a time, so the rest of a
/// very long line is never held in memory.
async fn skip_line<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn streams_lines_and_kills_the_group_on_timeout() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let (_tx, cancel) = watch::channel(false);
        let mut ok = command("/bin/sh");
        ok.args(["-c", "echo one; echo two >&2"]);
        let collected = lines.clone();
        let status = run(
            ok,
            Duration::from_secs(5),
            cancel.clone(),
            move |line, err| {
                collected.lock().unwrap().push((line, err));
                async { Ok(()) }
            },
        )
        .await
        .unwrap();
        assert!(status.success());
        let mut seen = lines.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, vec![("one".into(), false), ("two".into(), true)]);

        let mut slow = command("/bin/sh");
        slow.args(["-c", "sleep 30 & wait"]);
        let error = run(slow, Duration::from_millis(200), cancel, |_, _| async {
            Ok(())
        })
        .await
        .unwrap_err();
        assert!(matches!(error.downcast_ref(), Some(Stopped::TimedOut)));
    }

    #[tokio::test]
    async fn a_line_over_the_limit_is_cut_with_a_marker_and_the_run_goes_on() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let (_tx, cancel) = watch::channel(false);
        let mut long = command("/bin/sh");
        long.args([
            "-c",
            "head -c 40000 /dev/zero | tr '\\0' a; echo; echo after",
        ]);
        let collected = lines.clone();
        let status = run(long, Duration::from_secs(10), cancel, move |line, err| {
            collected.lock().unwrap().push((line, err));
            async { Ok(()) }
        })
        .await
        .unwrap();
        assert!(status.success());
        let seen = lines.lock().unwrap().clone();
        // The rest of the long line is dropped, not split into lines of its own.
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0].0.len(), MAX_LINE);
        assert!(seen[0].0.starts_with("aaaa") && seen[0].0.ends_with(TRUNCATED_MARK));
        assert_eq!(seen[1], ("after".to_string(), false));
    }

    #[tokio::test]
    async fn a_cut_line_stays_within_the_limit_on_a_character_boundary() {
        // Each character is two bytes, so the cut lands inside one. Lossy decoding of that
        // partial character would grow the line past the limit that CI log ingest enforces.
        let input = format!("{}\nafter\n", "é".repeat(10_000));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let collected = lines.clone();
        let on_line = move |line: String, _stderr: bool| {
            collected.lock().unwrap().push(line);
            async { Ok(()) }
        };
        forward(Some(input.as_bytes()), false, &on_line)
            .await
            .unwrap();
        let seen = lines.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[0].len() <= MAX_LINE, "{}", seen[0].len());
        assert!(seen[0].starts_with('é') && seen[0].ends_with(TRUNCATED_MARK));
        assert_eq!(seen[1], "after");
    }

    #[tokio::test]
    async fn callback_failure_cancel_timeout_and_abort_stop_descendants() {
        for reason in ["callback", "cancel", "timeout", "abort"] {
            let directory = tempfile::tempdir().unwrap();
            let pids = directory.path().join("pids");
            let mut child = command("/bin/sh");
            child.args(["-c", "sleep 60 & descendant=$!; printf '%s %s\\n' \"$$\" \"$descendant\" > \"$1\"; if [ \"$2\" = callback ]; then echo ready; fi; wait", "sh"]).arg(&pids).arg(reason);
            let (tx, cancel) = watch::channel(false);
            let duration = if reason == "timeout" {
                Duration::from_millis(200)
            } else {
                Duration::from_secs(10)
            };
            let task = tokio::spawn(run(child, duration, cancel, move |_, _| async move {
                if reason == "callback" {
                    Err(anyhow::anyhow!("log write failed"))
                } else {
                    Ok(())
                }
            }));
            let numbers = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(text) = tokio::fs::read_to_string(&pids).await {
                        let numbers: Vec<i32> = text
                            .split_whitespace()
                            .filter_map(|s| s.parse().ok())
                            .collect();
                        if numbers.len() == 2 {
                            break numbers;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let _cleanup =
                crate::backend::git::repository::process::ProcessGroup(Some(numbers[0] as u32));
            if matches!(reason, "cancel" | "abort") {
                for &pid in &numbers {
                    assert_eq!(
                        unsafe { libc::kill(pid, 0) },
                        0,
                        "positive liveness control"
                    );
                }
            }
            if reason == "abort" {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                if reason == "cancel" {
                    tx.send(true).unwrap();
                }
                let error = task.await.unwrap().unwrap_err();
                match reason {
                    "callback" => assert_eq!(error.to_string(), "log write failed"),
                    "cancel" => assert!(matches!(error.downcast_ref(), Some(Stopped::Cancelled))),
                    "timeout" => assert!(matches!(error.downcast_ref(), Some(Stopped::TimedOut))),
                    _ => unreachable!(),
                }
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while numbers
                    .iter()
                    .any(|&pid| unsafe { libc::kill(pid, 0) } == 0)
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{reason} left a parent or descendant alive: {numbers:?}"));
        }
    }

    #[tokio::test]
    async fn successful_parent_waits_for_redirected_background_descendant_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let marker = directory.path().join("delayed-marker");
        let mut child = command("/bin/sh");
        child
            .args([
                "-c",
                "(trap '' TERM; sleep 30; touch \"$2\") </dev/null >/dev/null 2>&1 & echo $! > \"$1\"",
                "sh",
            ])
            .arg(&pid_file)
            .arg(&marker);
        let (_tx, cancel) = watch::channel(false);
        let started = Instant::now();
        let status = run(child, Duration::from_secs(10), cancel, |_, _| async {
            Ok(())
        })
        .await
        .unwrap();
        assert!(status.success());
        assert!(started.elapsed() < TERM_GRACE + KILL_GRACE + Duration::from_secs(2));
        let pid: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !marker.exists(),
            "background descendant performed a delayed effect"
        );
    }

    #[tokio::test]
    async fn inherited_pipes_do_not_keep_a_successful_runner_open() {
        let mut child = command("/bin/sh");
        child.args(["-c", "sleep 60 & echo done"]);
        let (_tx, cancel) = watch::channel(false);
        let status = run(child, Duration::from_secs(10), cancel, |_, _| async {
            Ok(())
        })
        .await
        .unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn termination_is_graceful_then_forces_stubborn_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let term_marker = directory.path().join("term");
        let pids = directory.path().join("pids");
        let ready = [
            directory.path().join("ready-1"),
            directory.path().join("ready-2"),
        ];
        let mut child = command("/bin/sh");
        child
            .args([
                "-c",
                "(trap 'echo term > \"$1\"; exit 0' TERM; : > \"$3\"; while :; do sleep .1; done) & graceful=$!; (trap '' TERM; : > \"$4\"; while :; do sleep .1; done) & stubborn=$!; printf '%s %s\n' \"$graceful\" \"$stubborn\" > \"$2\"; wait",
                "sh",
            ])
            .arg(&term_marker)
            .arg(&pids)
            .args(&ready);
        let (tx, cancel) = watch::channel(false);
        // Cancel only once both traps are installed, however slow the machine is.
        let cancel_when_ready = async {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !(pids.exists() && ready.iter().all(|file| file.exists())) {
                assert!(Instant::now() < deadline, "shell never became ready");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let cancelled_at = Instant::now();
            tx.send(true).unwrap();
            cancelled_at
        };
        let (result, cancelled_at) = tokio::join!(
            run(child, Duration::from_secs(60), cancel, |_, _| async {
                Ok(())
            }),
            cancel_when_ready
        );
        let error = result.unwrap_err();
        assert!(matches!(error.downcast_ref(), Some(Stopped::Cancelled)));
        assert!(
            term_marker.exists(),
            "SIGTERM was not delivered before SIGKILL"
        );
        assert!(cancelled_at.elapsed() >= TERM_GRACE);
        assert!(cancelled_at.elapsed() < TERM_GRACE + KILL_GRACE + Duration::from_secs(2));
        for pid in std::fs::read_to_string(pids)
            .unwrap()
            .split_whitespace()
            .map(|pid| pid.parse::<i32>().unwrap())
        {
            assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
        }
    }

    #[tokio::test]
    async fn cancellation_stops_the_command() {
        let (tx, cancel) = watch::channel(false);
        let mut slow = command("/bin/sh");
        slow.args(["-c", "sleep 30"]);
        let running = tokio::spawn(run(slow, Duration::from_secs(30), cancel, |_, _| async {
            Ok(())
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send(true).unwrap();
        let error = running.await.unwrap().unwrap_err();
        assert!(matches!(error.downcast_ref(), Some(Stopped::Cancelled)));
    }
}

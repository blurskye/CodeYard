use super::*;

fn shell(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.process_group(0).kill_on_drop(true);
    command
}

#[tokio::test]
async fn streaming_delivers_eof_and_preserves_output_and_exit_status() {
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let status = stream(
        shell("cat; printf finished; printf warning >&2; exit 7"),
        b"input".as_slice(),
        &mut output,
        &mut errors,
        false,
    )
    .await
    .unwrap();
    assert_eq!(output, b"inputfinished");
    assert_eq!(errors, b"warning");
    assert_eq!(status.code(), Some(7));
}

#[tokio::test]
async fn streaming_idle_timeout_kills_the_process_group() {
    let (input, _keep_open) = tokio::io::duplex(CHUNK);
    let (output, reader) = tokio::io::duplex(CHUNK);
    let task = tokio::spawn(async move {
        // The idle clock starts at spawn, so the budget must outlast a slow shell start.
        stream_with_idle(
            shell("echo $$; exec sleep 60"),
            input,
            output,
            tokio::io::sink(),
            false,
            Duration::from_secs(2),
        )
        .await
    });
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
        .await
        .unwrap();
    let pid = line.trim().parse::<i32>().unwrap();
    let error = task.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("idle timeout"));
    for _ in 0..100 {
        // SAFETY: signal zero only checks whether the disposable child exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("idle Git process was not killed");
}

#[tokio::test]
async fn receive_finishes_after_output_disconnect() {
    let (output, reader) = tokio::io::duplex(CHUNK);
    drop(reader);
    let status = stream(
        shell("cat; printf accepted; exit 0"),
        b"pack".as_slice(),
        output,
        tokio::io::sink(),
        true,
    )
    .await
    .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn buffered_output_and_nonzero_exit_are_preserved() {
    let output = run(
        shell("cat; printf finished; printf warning >&2; exit 7"),
        b"input",
        1024,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(output.stdout, b"inputfinished");
    assert_eq!(output.stderr, b"warning");
    assert_eq!(output.status.code(), Some(7));
}

#[tokio::test]
async fn buffered_failure_timeout_and_abort_stop_parent_and_descendant() {
    for reason in ["limit", "timeout", "abort"] {
        let directory = tempfile::tempdir().unwrap();
        let pids = directory.path().join("pids");
        let mut command = shell("sleep 60 & descendant=$!; printf '%s %s\\n' \"$$\" \"$descendant\" > \"$1\"; if [ \"$2\" = limit ]; then printf excessive-output; fi; wait");
        command.arg("sh").arg(&pids).arg(reason);
        let limit = if reason == "limit" { 1 } else { 1024 };
        // The budget also covers waiting for one of the shared git slots, which other tests
        // in this binary may hold, so it is generous. The shell is killed when it runs out.
        let duration = if reason == "timeout" {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(10)
        };
        let task = tokio::spawn(async move { run(command, b"", limit, duration).await });
        let numbers = tokio::time::timeout(Duration::from_secs(10), async {
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
        // Also own cleanup if an assertion below fails.
        let _cleanup = ProcessGroup(Some(numbers[0] as u32));
        if reason == "abort" {
            for &pid in &numbers {
                assert_eq!(
                    unsafe { libc::kill(pid, 0) },
                    0,
                    "positive liveness control"
                );
            }
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            let error = task.await.unwrap().unwrap_err();
            assert!(
                error.to_string().contains(if reason == "limit" {
                    "output exceeded"
                } else {
                    "git timed out"
                }),
                "{error}"
            );
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
async fn a_busy_node_makes_a_session_wait_and_only_then_fails() {
    let slots = Arc::new(Semaphore::new(1));
    let held = slots.clone().acquire_owned().await.unwrap();
    let waiting = tokio::spawn(wait_for_slot(slots.clone(), Duration::from_secs(5)));
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(held);
    assert!(waiting.await.unwrap().is_ok());

    let _held = slots.clone().acquire_owned().await.unwrap();
    let error = wait_for_slot(slots, Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(error.is::<SessionsBusy>());
}

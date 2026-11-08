//! Starting the `codeyard` binary as a child process on free local ports.
use std::{
    fs::File,
    future::Future,
    net::{SocketAddr, TcpListener},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const ATTEMPTS: usize = 5;
/// How long a starting server may take before the test gives up on it. A loaded, shared runner
/// can take far longer than the builder; the loop below returns as soon as the server is ready.
const STARTUP_WATCHDOG: Duration = Duration::from_secs(180);

/// Starts the server `command` builds for `ports` free local addresses, and waits until `ready`
/// says it is serving. Its output goes to `log`.
///
/// A port found by binding and releasing it can be taken by another test before the server binds
/// it. The server then exits with "in use" and is started again on new ports.
pub async fn spawn_on_free_ports<Ready: Future<Output = bool>>(
    log: &Path,
    ports: usize,
    command: impl Fn(&[SocketAddr]) -> Command,
    ready: impl Fn(Vec<SocketAddr>) -> Ready,
) -> (Child, Vec<SocketAddr>) {
    for attempt in 1..=ATTEMPTS {
        let listeners: Vec<TcpListener> = (0..ports)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let addresses: Vec<SocketAddr> = listeners
            .iter()
            .map(|listener| listener.local_addr().unwrap())
            .collect();
        drop(listeners);
        let output = File::create(log).unwrap();
        let mut child = command(&addresses)
            // Unoptimized server futures need the same stack budget as the E2E servers.
            .env("RUST_MIN_STACK", "33554432")
            // Workspace test features also apply to the child binary's receive hooks.
            .envs(
                std::env::var_os("GITSERV_TEST_OPENSSH_HELPER")
                    .map(|helper| ("GITSERV_TEST_OPENSSH_HELPER", helper)),
            )
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + STARTUP_WATCHDOG;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                let text = std::fs::read_to_string(log).unwrap();
                if text.contains("in use") && attempt < ATTEMPTS {
                    break;
                }
                panic!("server exited with {status}: {text}");
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "server did not start within {STARTUP_WATCHDOG:?}: {}",
                    std::fs::read_to_string(log).unwrap()
                );
            };
            if matches!(
                tokio::time::timeout(left, ready(addresses.clone())).await,
                Ok(true)
            ) {
                return (child, addresses);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    unreachable!("the last attempt returns or panics")
}

//! Child-process and file-wait helpers for repository creation tests.

/// A child process gives this test its own Git executable without changing the
/// environment of other tests. Pause on both sides of the actual fork fetch.
pub async fn fork_process(test: &str) -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("FORK_GATE") {
        return Some(path.into());
    }
    use std::os::unix::fs::PermissionsExt;
    let gate = tempfile::tempdir().unwrap();
    let path = std::env::var_os("PATH").unwrap();
    let git = std::env::split_paths(&path)
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let wrapper = gate.path().join("git");
    std::fs::write(
        &wrapper,
        format!(
            r#"#!/bin/sh
export PATH='{}'
fetch=false
for arg do
    if [ "$arg" = fetch ]; then fetch=true; fi
done
wait_for() {{
    count=0
    while [ ! -e '{}/'"$1" ]; do
        count=$((count + 1))
        [ "$count" -lt 600 ] || exit 90
        sleep 0.05
    done
}}
if "$fetch"; then
    : > '{}/fetching'
    wait_for fetch
fi
'{}' "$@"
status=$?
if "$fetch" && [ "$status" = 0 ]; then
    : > '{}/copied'
    wait_for finish
fi
exit "$status"
"#,
            path.to_string_lossy(),
            gate.path().display(),
            gate.path().display(),
            git.display(),
            gate.path().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("FORK_GATE", gate.path())
            .env(
                "PATH",
                format!("{}:{}", gate.path().display(), path.to_string_lossy()),
            )
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    // A name that matches no test would exit 0 with zero tests run, so require the one test.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    None
}

pub async fn wait_for_file(path: &std::path::Path) {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Git reached the fork gate");
}

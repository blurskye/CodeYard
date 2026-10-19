#![cfg(all(not(target_arch = "wasm32"), unix))]

use codeyard::backend::db;
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::path::Path;
use std::process::{Command, Output};

fn run_as_with_env(
    directory: &Path,
    arguments: &[&str],
    identity: Option<(u32, u32)>,
    environment: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codeyard"));
    command
        .args(arguments)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .envs(environment.iter().copied())
        .current_dir(directory);
    if let Some((uid, gid)) = identity {
        command.uid(uid).gid(gid);
    }
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o022);
            Ok(())
        });
    }
    command.output().unwrap()
}

fn run_as(directory: &Path, arguments: &[&str], identity: Option<(u32, u32)>) -> Output {
    run_as_with_env(directory, arguments, identity, &[])
}

fn run(directory: &Path, arguments: &[&str]) -> Output {
    run_as(directory, arguments, None)
}

fn run_with_env(directory: &Path, arguments: &[&str], environment: &[(&str, &str)]) -> Output {
    run_as_with_env(directory, arguments, None, environment)
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_sqlite_files_private(database: &Path) {
    assert_eq!(mode(database), 0o600, "{}", database.display());
    for suffix in ["-wal", "-shm"] {
        let mut name = database.as_os_str().to_owned();
        name.push(suffix);
        let sidecar = std::path::PathBuf::from(name);
        if sidecar.exists() {
            assert_eq!(mode(&sidecar), 0o600, "{}", sidecar.display());
        }
    }
}

#[test]
fn real_cli_creates_and_repairs_private_native_storage_under_umask_022() {
    let installation = tempfile::tempdir().unwrap();
    assert_success(&run(installation.path(), &["init"]));
    let mut bootstrap = Command::new(env!("CARGO_BIN_EXE_codeyard"))
        .args(["user", "bootstrap", "root_admin", "--password-stdin"])
        .env_clear()
        .current_dir(installation.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        &mut bootstrap.stdin.take().unwrap(),
        b"bootstrap-password\n",
    )
    .unwrap();
    assert_success(&bootstrap.wait_with_output().unwrap());
    assert_success(&run(
        installation.path(),
        &["user", "create", "privacy-owner"],
    ));
    assert_success(&run(
        installation.path(),
        &[
            "repo",
            "create",
            "privacy-owner",
            "private-one",
            "--private",
        ],
    ));

    let state = installation.path().join("data");
    let database = state.join("gitserv.db");
    let repos = installation.path().join("repos");
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(&repos), 0o700);
    assert_sqlite_files_private(&database);

    let repository_file = std::fs::read_dir(&repos)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("HEAD");
    std::fs::set_permissions(&repository_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&repos, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_success(&run(
        installation.path(),
        &[
            "repo",
            "create",
            "privacy-owner",
            "private-two",
            "--private",
        ],
    ));
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(&repos), 0o700);
    assert_sqlite_files_private(&database);
    assert_eq!(
        mode(&repository_file),
        0o644,
        "repository contents are not recursively changed"
    );

    let external_parent = installation.path().join("operator-owned");
    std::fs::create_dir(&external_parent).unwrap();
    std::fs::set_permissions(&external_parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let external = external_parent.join("external.sqlite");
    std::fs::write(&external, []).unwrap();
    std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o644)).unwrap();
    let url = format!("sqlite://{}?mode=rwc", external.display());
    assert_success(&run_with_env(
        installation.path(),
        &["--state-dir", "external-state", "init"],
        &[("DATABASE_URL", &url)],
    ));
    assert_sqlite_files_private(&external);
    assert_eq!(
        mode(&external_parent),
        0o755,
        "operator parent mode changed"
    );

    if unsafe { libc::geteuid() } == 0 {
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg("test ! -r \"$1\" && test ! -x \"$2\"")
            .arg("privacy-check")
            .arg(&database)
            .arg(&repos)
            .uid(65534)
            .gid(65534)
            .status()
            .unwrap();
        assert!(status.success());
    }
}

#[test]
fn default_sqlite_database_uses_the_literal_state_path() {
    let installation = tempfile::tempdir().unwrap();
    std::fs::write(installation.path().join(".env"), "").unwrap();
    let decoded = installation.path().join("forge state");
    std::fs::create_dir(&decoded).unwrap();
    for name in ["forge%20state", "forge?state", "forge#state"] {
        assert_success(&run(installation.path(), &["--state-dir", name, "init"]));
        assert_sqlite_files_private(&installation.path().join(name).join("gitserv.db"));
    }
    assert!(!decoded.join("gitserv.db").exists());
}

#[test]
fn database_urls_and_join_tickets_are_refused_in_argv_without_echoing_them() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = "postgres://operator:argv-password@database.invalid/forge";
    let output = run(directory.path(), &["--database-url", database_url, "serve"]);
    assert!(!output.status.success());
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(message.contains("DATABASE_URL"), "{message}");
    assert!(
        !message.contains(database_url),
        "database URL leaked: {message}"
    );
    assert!(
        !message.contains("argv-password"),
        "password leaked: {message}"
    );

    let configured = tempfile::tempdir().unwrap();
    let invalid_url = "postgres://operator:environment-password@[not-an-ipv6-address]/forge";
    let output = run_with_env(
        configured.path(),
        &["init"],
        &[("DATABASE_URL", invalid_url)],
    );
    assert!(!output.status.success());
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        message.contains("configured application database"),
        "{message}"
    );
    assert!(
        !message.contains(invalid_url),
        "database URL leaked: {message}"
    );
    assert!(
        !message.contains("environment-password"),
        "password leaked: {message}"
    );

    let ticket = "codeyard-join-v1.argv-ticket-must-not-appear";
    let output = run(directory.path(), &["cluster", "join", ticket]);
    assert!(!output.status.success());
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(message.contains("CLUSTER_JOIN_TOKEN"), "{message}");
    assert!(!message.contains(ticket), "join ticket leaked: {message}");

    let output = run_with_env(
        directory.path(),
        &["cluster", "join"],
        &[("CLUSTER_JOIN_TOKEN", ticket)],
    );
    assert!(!output.status.success());
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(message.contains("not a Codeyard join ticket"), "{message}");
    assert!(
        !message.contains(ticket),
        "environment ticket leaked: {message}"
    );
    assert!(!directory.path().join("data").exists());
}

#[test]
fn native_startup_rejects_s3_on_the_session_cookie_host_before_contacting_it() {
    let directory = tempfile::tempdir().unwrap();
    let public_endpoint = "https://localhost:19443";
    let secret_key = "s3-secret-must-not-appear";
    let output = run_with_env(
        directory.path(),
        &["serve"],
        &[
            ("PUBLIC_URL", "http://localhost:13000"),
            ("SSH_MODE", "disabled"),
            ("HTTP_ADDR", "127.0.0.1:0"),
            ("S3_ENDPOINT", "http://storage.invalid:9000"),
            ("S3_PUBLIC_ENDPOINT", public_endpoint),
            ("S3_ACCESS_KEY", "fixture-access"),
            ("S3_SECRET_KEY", secret_key),
            ("S3_BUCKET", "fixture-bucket"),
        ],
    );
    assert!(!output.status.success());
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(message.contains("session cookie scope"), "{message}");
    assert!(
        !message.contains(public_endpoint),
        "endpoint leaked: {message}"
    );
    assert!(!message.contains(secret_key), "S3 key leaked: {message}");
    assert!(!message.contains("Git HTTP server starting"), "{message}");
}

#[tokio::test]
async fn direct_sqlite_urls_secure_main_and_created_sidecars_without_touching_memory() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("query.sqlite");
    let url = format!("sqlite://{}?mode=rwc", database.display());
    let pool = db::init_db(&url).await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    assert_sqlite_files_private(&database);
    pool.close().await;

    let generated_memory_files = || {
        std::fs::read_dir(".")
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().starts_with("file:sqlx-in-memory-"))
            .collect::<std::collections::BTreeSet<_>>()
    };
    let before_memory = generated_memory_files();
    let memory = db::init_db("sqlite::memory:").await.unwrap();
    memory.ping().await.unwrap();
    memory.close().await;
    assert_eq!(generated_memory_files(), before_memory);
    let memory_url = format!(
        "sqlite://{}?mode=memory&cache=shared",
        root.path().join("private-memory").display()
    );
    let named_memory = db::init_db(&memory_url).await.unwrap();
    named_memory.ping().await.unwrap();
    named_memory.close().await;
    assert!(!root.path().join("private-memory").exists());
}

#[test]
fn dotenv_errors_fail_before_default_state_but_missing_is_optional() {
    let malformed = tempfile::tempdir().unwrap();
    std::fs::write(malformed.path().join(".env"), "SECRET='unterminated\n").unwrap();
    let output = run(malformed.path(), &["init"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("could not load .env"), "{stderr}");
    assert!(
        !stderr.contains("SECRET"),
        "dotenv contents leaked: {stderr}"
    );
    assert!(!malformed.path().join("data/gitserv.db").exists());

    let unreadable = tempfile::tempdir().unwrap();
    let unreadable_env = unreadable.path().join(".env");
    std::fs::write(&unreadable_env, "NOT_PRINTED=value\n").unwrap();
    std::fs::set_permissions(&unreadable_env, std::fs::Permissions::from_mode(0o000)).unwrap();
    let identity = if unsafe { libc::geteuid() } == 0 {
        std::fs::set_permissions(unreadable.path(), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        Some((65534, 65534))
    } else {
        None
    };
    let output = run_as(unreadable.path(), &["init"], identity);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not load .env"));
    assert!(!unreadable.path().join("data/gitserv.db").exists());

    let missing = tempfile::tempdir().unwrap();
    assert_success(&run(missing.path(), &["init"]));
    assert!(missing.path().join("data/gitserv.db").exists());
}

#[test]
fn openssh_helper_dispatch_does_not_load_dotenv_or_create_state() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join(".env"), "SECRET='unterminated\n").unwrap();
    let socket = directory.path().join("missing.sock");
    let output = run(
        directory.path(),
        &[
            "ssh",
            "serve",
            "--key-id",
            "1",
            "--socket",
            socket.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("could not load .env"));
    assert!(!directory.path().join("data").exists());
}

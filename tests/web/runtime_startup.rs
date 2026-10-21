//! Recovery and logging through a running standalone server on both databases.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::native_database as native;
use crate::support::server::spawn_on_free_ports;

use codeyard::backend::db;
use reqwest::{Client, StatusCode};
use russh::keys::{ssh_key::private::Ed25519Keypair, ssh_key::LineEnding, PrivateKey};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Server {
    child: Child,
    base: String,
    ssh_port: u16,
    log: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    async fn start(root: &Path, url: &str) -> Self {
        let log = root.join("server.log");
        let client = Client::new();
        let (child, addresses) = spawn_on_free_ports(
            &log,
            2,
            |addresses| {
                let mut command = Command::new(env!("CARGO_BIN_EXE_codeyard"));
                command
                    .env_clear()
                    .env("PATH", std::env::var_os("PATH").unwrap())
                    .env("HOME", root)
                    .env("WEBUI_DIST", root.join("webui"))
                    .env("DATABASE_URL", url)
                    .env("GITSERV_STATE_DIR", root.join("state"))
                    .env("REPOS_PATH", root.join("repos"))
                    .args(["serve", "--roles", "web,storage,ssh"])
                    .args(["--ssh-mode", "builtin"])
                    .args(["--http-addr", &addresses[0].to_string()])
                    .args(["--ssh-addr", &addresses[1].to_string()])
                    .current_dir(root);
                command
            },
            |addresses| {
                let client = &client;
                async move {
                    client
                        .get(format!("http://{}/ready", addresses[0]))
                        .send()
                        .await
                        .is_ok_and(|reply| reply.status().is_success())
                        && tokio::net::TcpStream::connect(addresses[1]).await.is_ok()
                }
            },
        )
        .await;
        Self {
            child,
            base: format!("http://{}", addresses[0]),
            ssh_port: addresses[1].port(),
            log,
        }
    }
}

async fn bundle_recovery(url: &str) {
    let root = tempfile::tempdir().unwrap();
    let server = Server::start(root.path(), url).await;
    let client = Client::new();
    let bundle = root.path().join("webui");
    // First the directory is absent, then the index, then its referenced module.
    for step in 0..3 {
        for (path, status) in [
            ("/setup", StatusCode::SERVICE_UNAVAILABLE),
            ("/assets/fixture.js", StatusCode::NOT_FOUND),
        ] {
            assert_eq!(
                client
                    .get(format!("{}{path}", server.base))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                status,
                "step {step}: {path}"
            );
        }
        match step {
            0 => std::fs::create_dir_all(bundle.join("assets")).unwrap(),
            1 => std::fs::write(
                bundle.join("index.html"),
                "<script type=\"module\" src=\"/assets/fixture.js\"></script>",
            )
            .unwrap(),
            _ => std::fs::write(bundle.join("assets/fixture.js"), "/* repaired */").unwrap(),
        }
    }
    let response = client
        .get(format!("{}/assets/fixture.js", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/javascript; charset=utf-8"
    );
    assert_eq!(response.text().await.unwrap(), "/* repaired */");
    let response = client
        .get(format!("{}/setup", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-codeyard-rendering"], "ssr-hydrated");
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("/assets/fixture.js"));
    // A successful initialization stays cached, even if the generated index goes away.
    std::fs::remove_file(bundle.join("index.html")).unwrap();
    assert_eq!(
        client
            .get(format!("{}/setup", server.base))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

async fn rejected_commands(pool: &db::DbPool, url: &str) {
    let root = tempfile::tempdir().unwrap();
    let user = db::users::accounts::create_user(pool, "loguser", None)
        .await
        .unwrap();
    let key = PrivateKey::from(Ed25519Keypair::from_seed(&[42; 32]));
    db::users::ssh_keys::add_ssh_key(
        pool,
        user.id,
        "log fixture",
        &key.public_key().to_openssh().unwrap(),
    )
    .await
    .unwrap();
    let key_path = root.path().join("client_key");
    std::fs::write(
        &key_path,
        key.to_openssh(LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = Server::start(root.path(), url).await;
    let ssh = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .chain([PathBuf::from("/run/current-system/sw/bin")])
        .map(|path| path.join("ssh"))
        .find(|path| path.is_file())
        .expect("OpenSSH client");
    for command in [
        "not-git\nERROR forged-entry".to_owned(),
        format!("{}\nERROR oversized-entry", "x".repeat(4096)),
        "git-upload-pack 'loguser/missing.git'".to_owned(),
    ] {
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(&ssh)
                .args([
                    "-F",
                    "/dev/null",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "IdentitiesOnly=yes",
                    "-o",
                    "IdentityAgent=none",
                    "-o",
                    "StrictHostKeyChecking=no",
                    "-o",
                    "UserKnownHostsFile=/dev/null",
                    "-p",
                    &server.ssh_port.to_string(),
                    "-i",
                ])
                .arg(&key_path)
                .arg("git@127.0.0.1")
                .arg(&command)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!output.status.success(), "{output:?}");
    }
    let log = std::fs::read_to_string(&server.log).unwrap();
    assert!(
        log.contains("Git command for loguser/missing (write=false)"),
        "{log}"
    );
    for text in [
        "Exec request:",
        "forged-entry",
        "oversized-entry",
        "not-git",
    ] {
        assert!(!log.contains(text), "{log}");
    }
}

#[tokio::test]
async fn sqlite_bundle_recovers_after_failed_initialization() {
    let database = native::NativeDatabase::new().await;
    bundle_recovery(&database.url).await;
    database.close().await;
}

#[tokio::test]
async fn postgres_bundle_recovers_after_failed_initialization() {
    let database = native::PostgresDatabase::new().await;
    bundle_recovery(&database.connection_url()).await;
}

#[tokio::test]
async fn sqlite_rejected_ssh_commands_do_not_enter_logs() {
    let database = native::NativeDatabase::new().await;
    rejected_commands(&database.pool, &database.url).await;
    database.close().await;
}

#[tokio::test]
async fn postgres_rejected_ssh_commands_do_not_enter_logs() {
    let database = native::PostgresDatabase::new().await;
    rejected_commands(&database.pool, &database.connection_url()).await;
}

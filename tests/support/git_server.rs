//! The `codeyard` server as a child process, and a Git client that sends a token, for tests
//! that speak smart HTTP the way a real client does.
use crate::support::fake_s3::FakeS3;
use crate::support::native_database as native;
use crate::support::server::spawn_on_free_ports;
use base64::{engine::general_purpose::STANDARD, Engine};

use reqwest::Client;
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    process::{Child, Command, Output, Stdio},
};

pub struct Server {
    pub child: Child,
    pub base: String,
    pub log: std::path::PathBuf,
    _s3: Option<FakeS3>,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    pub async fn start(root: &Path, url: &str) -> Self {
        // Tests that fill every session slot count on 16 of them and an instant refusal.
        Self::start_with_session_slots(root, url, 16, 0).await
    }

    /// A server with `slots` Git sessions at once. A request that finds them all taken waits up
    /// to `wait_seconds` for one before it is refused as busy.
    pub async fn start_with_session_slots(
        root: &Path,
        url: &str,
        slots: usize,
        wait_seconds: u64,
    ) -> Self {
        Self::start_configured(root, url, slots, wait_seconds, None).await
    }

    pub async fn start_with_master_key(root: &Path, url: &str, key: &str) -> Self {
        Self::start_configured(root, url, 16, 0, Some(key)).await
    }

    async fn start_configured(
        root: &Path,
        url: &str,
        slots: usize,
        wait_seconds: u64,
        master_key: Option<&str>,
    ) -> Self {
        let log = root.join("server.log");
        let pool = codeyard_database::init_db(url).await.unwrap();
        let s3 = FakeS3::for_database(&pool).await;
        pool.close().await;
        let client = Client::new();
        let (child, addresses) = spawn_on_free_ports(
            &log,
            1,
            |addresses| {
                let address = addresses[0].to_string();
                let mut command = Command::new(env!("CARGO_BIN_EXE_codeyard"));
                command
                    .env_clear()
                    .env("PATH", std::env::var_os("PATH").unwrap())
                    .env("HOME", root)
                    .env("WEBUI_DIST", std::env::var_os("WEBUI_DIST").unwrap())
                    .env("RUST_LOG", "info")
                    .env("GIT_SESSION_LIMIT", slots.to_string())
                    .env("GIT_SESSION_WAIT_SECONDS", wait_seconds.to_string())
                    .env("COOKIE_SECURE", "false")
                    .env("DATABASE_URL", url)
                    .env("GITSERV_STATE_DIR", root.join("state"))
                    .env("REPOS_PATH", root.join("repos"))
                    .env("PUBLIC_URL", format!("http://{address}"))
                    .args(["serve", "--roles", "server", "--ssh-mode", "disabled"])
                    .args(["--http-addr", &address])
                    .current_dir(root);
                if let Some(s3) = &s3 {
                    command.envs(s3.environment());
                }
                if let Some(key) = master_key {
                    command.env("SECRETS_MASTER_KEY", key);
                }
                command
            },
            |addresses| {
                let client = &client;
                async move {
                    client
                        .get(format!("http://{}/health", addresses[0]))
                        .send()
                        .await
                        .is_ok_and(|reply| reply.status().is_success())
                }
            },
        )
        .await;
        Self {
            child,
            base: format!("http://{}", addresses[0]),
            log,
            _s3: s3,
        }
    }
}

pub fn git(root: &Path, arguments: &[&str], token: Option<&str>) -> Output {
    let mut command = Command::new("git");
    command
        .env("HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_TRACE")
        .env_remove("GIT_TRACE_CURL")
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "http@example.invalid")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "http@example.invalid")
        .arg("-c")
        .arg("http.postBuffer=1024");
    if let Some(token) = token {
        command.arg("-c").arg(format!(
            "http.extraHeader=Authorization: Basic {}",
            STANDARD.encode(format!("any-username:{token}"))
        ));
    }
    command
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}
pub fn success(output: Output) {
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Writes a token for `owner` that can push, stored the way the server looks tokens up.
pub async fn write_token(pool: &codeyard_database::DbPool, owner: i64) -> String {
    let token = native::token();
    let stored = hex::encode(Sha256::digest(token.as_bytes()));
    let session = native::token();
    codeyard_db_identity::users::sessions::create_session(
        pool,
        &session,
        owner,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    codeyard_db_identity::users::tokens::create_token(
        pool, owner, &session, "push", &stored, "write", None,
    )
    .await
    .unwrap()
    .unwrap();
    token
}

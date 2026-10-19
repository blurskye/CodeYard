//! Built-in SSH and HTTP test servers and the Git and API calls they take in tests.

use super::native_database as native;
use base64::{engine::general_purpose::STANDARD, Engine};
use codeyard::backend::{cluster::NodeKey, db};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::{Child, Output, Stdio},
    time::Duration,
};
use tokio::process::Command;

pub struct Server {
    pub child: Child,
    pub base: String,
    pub ssh_port: u16,
    pub log: PathBuf,
    pub node_id: i64,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    pub async fn start(
        root: &Path,
        database: &native::PostgresDatabase,
        name: &str,
        roles: &str,
        trusted_actor: i64,
    ) -> Self {
        std::fs::create_dir_all(root).unwrap();
        let http = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = http.local_addr().unwrap();
        let ssh = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ssh_address = ssh.local_addr().unwrap();
        drop((http, ssh));
        let log = root.join("server.log");
        let output = std::fs::File::create(&log).unwrap();
        let key = NodeKey::generate();
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_codeyard"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", root)
            .env("WEBUI_DIST", std::env::var_os("WEBUI_DIST").unwrap())
            .env("RUST_LOG", "info")
            .env("COOKIE_SECURE", "false")
            .env("DATABASE_URL", database.connection_url())
            .env("GITSERV_STATE_DIR", root.join("state"))
            .env("REPOS_PATH", root.join("repos"))
            .env("PUBLIC_URL", format!("http://{address}"))
            .env("CI_TRUSTED_ACTOR_IDS", trusted_actor.to_string())
            .env("NODE_NAME", name)
            .env("NODE_KEY", key.secret_hex())
            .env("IROH_BIND", "127.0.0.1:0")
            .args([
                "serve",
                "--roles",
                roles,
                "--ssh-mode",
                if roles.contains("ssh") {
                    "builtin"
                } else {
                    "disabled"
                },
                "--http-addr",
                &address.to_string(),
                "--ssh-addr",
                &ssh_address.to_string(),
            ])
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            base: format!("http://{address}"),
            ssh_port: ssh_address.port(),
            log,
            node_id: 0,
        };
        let client = Client::new();
        for _ in 0..100 {
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "server exited: {}",
                std::fs::read_to_string(&server.log).unwrap()
            );
            if let Some(node) = db::cluster::get_node_by_name(&database.pool, name)
                .await
                .unwrap()
            {
                if node.addr_hint.is_some()
                    && (!roles.contains("web")
                        || client
                            .get(format!("{}/health", server.base))
                            .send()
                            .await
                            .is_ok_and(|reply| reply.status().is_success()))
                {
                    server.node_id = node.id;
                    return server;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "server did not start: {}",
            std::fs::read_to_string(&server.log).unwrap()
        );
    }
}

/// `name` from PATH, or from the NixOS system profile (the build machine's PATH lacks ssh).
pub fn tool(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .chain([PathBuf::from("/run/current-system/sw/bin")])
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{name} is required"))
}

pub async fn git(root: &Path, args: &[&str], token: Option<&str>, key: &Path) -> Output {
    let mut command = Command::new("git");
    command.env("HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "remote@example.invalid")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "remote@example.invalid")
        .env("GIT_SSH_COMMAND", format!("{} -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none -o ConnectTimeout=5 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i {}", tool("ssh").display(), key.display()))
        .env_remove("GIT_TRACE")
        .env_remove("GIT_TRACE_CURL");
    if let Some(token) = token {
        command.args([
            "-c",
            &format!(
                "http.extraHeader=Authorization: Basic {}",
                STANDARD.encode(format!("git:{token}"))
            ),
        ]);
    }
    tokio::time::timeout(
        Duration::from_secs(180),
        command
            .args(args)
            .current_dir(root)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("Git client timed out")
    .unwrap()
}
pub fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
pub async fn api(
    client: &Client,
    server: &Server,
    session: (&str, &str),
    method: reqwest::Method,
    path: &str,
    value: Value,
    status: StatusCode,
) -> Value {
    let response = client
        .request(method, format!("{}{path}", server.base))
        .header("Cookie", session.0)
        .header("x-csrf-token", session.1)
        .json(&value)
        .send()
        .await
        .unwrap();
    let actual = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(actual, status, "{path}: {body}");
    serde_json::from_str(&body).unwrap()
}

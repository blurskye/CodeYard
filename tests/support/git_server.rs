//! The `codeyard` server as a child process, and a Git client that sends a token, for tests
//! that speak smart HTTP the way a real client does.
// Each integration crate uses a different subset of these shared test fixtures.
#![allow(dead_code)]
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::Client;
use std::{
    path::Path,
    process::{Child, Command, Output, Stdio},
};

pub struct Server {
    pub child: Child,
    pub base: String,
    pub log: std::path::PathBuf,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    pub async fn start(root: &Path, url: &str) -> Self {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        drop(socket);
        let log = root.join("server.log");
        let output = std::fs::File::create(&log).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_codeyard"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", root)
            .env("WEBUI_DIST", std::env::var_os("WEBUI_DIST").unwrap())
            .env("RUST_LOG", "info")
            .env("COOKIE_SECURE", "false")
            .env("DATABASE_URL", url)
            .env("GITSERV_STATE_DIR", root.join("state"))
            .env("REPOS_PATH", root.join("repos"))
            .env("PUBLIC_URL", format!("http://{address}"))
            .args([
                "serve",
                "--roles",
                "web,storage",
                "--ssh-mode",
                "disabled",
                "--http-addr",
                &address.to_string(),
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
            log,
        };
        let client = Client::new();
        for _ in 0..100 {
            if client
                .get(format!("{}/health", server.base))
                .send()
                .await
                .is_ok_and(|reply| reply.status().is_success())
            {
                return server;
            }
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "server exited: {}",
                std::fs::read_to_string(&server.log).unwrap()
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("server did not start");
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

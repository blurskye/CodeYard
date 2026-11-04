//! The real gh binary talks to the real router over its supported Unix-socket transport.
use crate::support::{database::Database, github_spec::assert_github_schema, http_client::Client};
use axum::{
    body::{to_bytes, Body},
    extract::Request,
    middleware::Next,
    response::Response,
};
use serde_json::Value;
use std::{io::Read, path::Path, process::Stdio, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream, UnixListener},
    process::{Child, Command},
    sync::mpsc,
};

struct Server(Vec<tokio::task::JoinHandle<()>>);
impl Drop for Server {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

fn gh(config: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("gh");
    command
        .args(args)
        .env("GH_CONFIG_DIR", config)
        .env("HOME", config)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_BROWSER", "true")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_ENTERPRISE_TOKEN")
        .env_remove("GITHUB_ENTERPRISE_TOKEN")
        .env_remove("GH_HOST")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn success(child: Child) -> std::process::Output {
    let output = tokio::time::timeout(Duration::from_secs(45), child.wait_with_output())
        .await
        .expect("gh finished")
        .unwrap();
    assert!(
        output.status.success(),
        "gh failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let actor = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let user = Client::login(&router, f.pool(), actor).await;
    let (sender, mut codes) = mpsc::unbounded_channel::<Value>();
    let observed = router.layer(axum::middleware::from_fn(
        move |request: Request, next: Next| {
            let sender = sender.clone();
            async move {
                let path = request.uri().path().to_owned();
                let request = if path == "/login/device/code" {
                    let (parts, body) = request.into_parts();
                    let bytes = to_bytes(body, 65536).await.unwrap();
                    let fields = url::form_urlencoded::parse(&bytes)
                        .into_owned()
                        .collect::<std::collections::HashMap<_, _>>();
                    assert_eq!(
                        fields["client_id"],
                        codeyard::backend::model::identity::CLI_CLIENT
                    );
                    for scope in ["repo", "read:org", "gist"] {
                        assert!(fields["scope"].split_whitespace().any(|s| s == scope));
                    }
                    Request::from_parts(parts, Body::from(bytes))
                } else {
                    request
                };
                let response = next.run(request).await;
                if !matches!(
                    path.as_str(),
                    "/login/device/code" | "/api/v3/user" | "/api/v3/" | "/api/v3"
                ) {
                    return response;
                }
                let (parts, body) = response.into_parts();
                let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
                let inspected = if parts
                    .headers
                    .get("content-encoding")
                    .is_some_and(|encoding| encoding == "gzip")
                {
                    let mut decoded = Vec::new();
                    flate2::read::GzDecoder::new(bytes.as_ref())
                        .read_to_end(&mut decoded)
                        .unwrap();
                    decoded
                } else {
                    bytes.to_vec()
                };
                if path == "/login/device/code" {
                    let value: Value = if parts.headers["content-type"] == "application/json" {
                        serde_json::from_slice(&inspected).unwrap()
                    } else {
                        serde_json::to_value(
                            url::form_urlencoded::parse(&inspected)
                                .into_owned()
                                .collect::<std::collections::HashMap<_, _>>(),
                        )
                        .unwrap()
                    };
                    sender.send(value).unwrap();
                } else {
                    let value: Value = serde_json::from_slice(&inspected).unwrap();
                    assert_github_schema(
                        if path == "/api/v3/user" {
                            "users/get-authenticated"
                        } else {
                            "meta/root"
                        },
                        parts.status.as_u16(),
                        &value,
                    );
                }
                Response::from_parts(parts, Body::from(bytes))
            }
        },
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let socket = root.path().join("http.sock");
    let unix = UnixListener::bind(&socket).unwrap();
    let _server = Server(vec![
        tokio::spawn(async move {
            axum::serve(listener, observed).await.unwrap();
        }),
        tokio::spawn(async move {
            loop {
                let (mut incoming, _) = unix.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut outgoing = TcpStream::connect(address).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
                });
            }
        }),
    ]);
    let config = root.path().join("gh");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(
        config.join("config.yml"),
        format!("http_unix_socket: {}\n", socket.display()),
    )
    .unwrap();
    let login = gh(
        &config,
        &[
            "auth",
            "login",
            "--hostname",
            "codeyard.test",
            "--web",
            "--git-protocol",
            "https",
            "--insecure-storage",
        ],
    )
    .spawn()
    .expect("the builder test environment must provide gh");
    let code = tokio::time::timeout(Duration::from_secs(15), codes.recv())
        .await
        .expect("gh requested device code")
        .unwrap();
    assert!(code["user_code"].is_string(), "Device response: {code}");
    let reply = user
        .form_reply(
            "/login/device",
            &[
                ("user_code", code["user_code"].as_str().unwrap()),
                ("decision", "approve"),
            ],
        )
        .await;
    assert!(
        reply.text().contains("Device authorized"),
        "{}",
        reply.text()
    );
    success(login).await;
    let status = success(
        gh(&config, &["auth", "status", "--hostname", "codeyard.test"])
            .spawn()
            .unwrap(),
    )
    .await;
    assert!(
        String::from_utf8_lossy(&status.stderr).contains("alice")
            || String::from_utf8_lossy(&status.stdout).contains("alice")
    );
    let original = success(
        gh(&config, &["auth", "token", "--hostname", "codeyard.test"])
            .spawn()
            .unwrap(),
    )
    .await;
    let original = String::from_utf8(original.stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert!(!original.is_empty());
    let refresh = gh(
        &config,
        &[
            "auth",
            "refresh",
            "--hostname",
            "codeyard.test",
            "--scopes",
            "workflow",
            "--insecure-storage",
        ],
    )
    .spawn()
    .unwrap();
    let code = tokio::time::timeout(Duration::from_secs(15), codes.recv())
        .await
        .expect("gh refresh requested device code")
        .unwrap();
    assert!(code["user_code"].is_string(), "Device response: {code}");
    let reply = user
        .form_reply(
            "/login/device",
            &[
                ("user_code", code["user_code"].as_str().unwrap()),
                ("decision", "approve"),
            ],
        )
        .await;
    assert!(
        reply.text().contains("Device authorized"),
        "{}",
        reply.text()
    );
    success(refresh).await;
    let refreshed = success(
        gh(&config, &["auth", "token", "--hostname", "codeyard.test"])
            .spawn()
            .unwrap(),
    )
    .await;
    let refreshed = String::from_utf8(refreshed.stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert_ne!(original, refreshed);
    let (headers, _) = super::github::request(
        &user.router,
        "users/get-authenticated",
        "GET",
        "/api/v3/user",
        Some(&format!("Bearer {refreshed}")),
        Value::Null,
        200,
    )
    .await;
    assert!(headers["x-oauth-scopes"]
        .to_str()
        .unwrap()
        .contains("workflow"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_oauth_cli_login_status_token_refresh() {
    contract(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_oauth_cli_login_status_token_refresh() {
    contract(true).await;
}

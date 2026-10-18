//! Real Git clients use a web/SSH node whose repositories live on a storage node.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::fake_webui_bundle as bundle;
use crate::support::native_database as native;
#[path = "../support/git_streaming.rs"]
mod streaming;

#[tokio::test]
async fn local_git_streams_large_packs_and_multi_round_fetches() {
    streaming::exercise(false).await;
}

#[tokio::test]
async fn remote_git_streams_large_packs_and_ends_on_storage_disconnect() {
    streaming::exercise(true).await;
}

use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{Duration as ChronoDuration, Utc};
use codeyard::backend::{
    cluster::{
        self,
        rpc::{self, Operation, Request},
        NodeKey,
    },
    db::{self, repositories::BranchProtectionRuleUpsert, users::tokens},
    repositories::git_sessions::ReceiveCredential,
    repositories::store::{GitService, GitSession, GitStreams, RepoError},
};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::{Child, Output, Stdio},
    time::Duration,
};
use tokio::process::Command;

struct Server {
    child: Child,
    base: String,
    ssh_port: u16,
    log: PathBuf,
    node_id: i64,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    async fn start(
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
fn tool(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .chain([PathBuf::from("/run/current-system/sw/bin")])
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{name} is required"))
}

async fn git(root: &Path, args: &[&str], token: Option<&str>, key: &Path) -> Output {
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
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
async fn api(
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

async fn refused_receive(
    context: &cluster::NodeContext,
    node: i64,
    repository: uuid::Uuid,
    credential: ReceiveCredential,
    owner: &str,
) -> RepoError {
    let mut session = GitSession::new(GitService::ReceivePack, true);
    session.credential = Some(credential);
    session.owner = owner.into();
    context
        .git_session(
            node,
            Request {
                repository,
                operation: Operation::GitSession(session),
                actor: None,
                ci_run: None,
            },
            GitStreams {
                input: &mut b"0000".as_slice(),
                output: &mut tokio::io::sink(),
                errors: &mut tokio::io::sink(),
            },
        )
        .await
        .unwrap_err()
        .downcast::<RepoError>()
        .unwrap()
}

#[tokio::test]
async fn remote_http_and_builtin_ssh_push_clone_fetch_and_receive_hooks() {
    bundle::install();
    let database = native::PostgresDatabase::new().await;
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let owner = db::users::accounts::create_user(&database.pool, "remote_owner", None)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let key_path = root.path().join("key");
    let keygen = Command::new(tool("ssh-keygen"))
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key_path)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(keygen.status.success());
    let key = db::users::ssh_keys::add_ssh_key(
        &database.pool,
        owner.id,
        "remote",
        &std::fs::read_to_string(key_path.with_extension("pub")).unwrap(),
    )
    .await
    .unwrap();
    let session = native::token();
    db::users::sessions::create_session(
        &database.pool,
        &session,
        owner.id,
        None,
        None,
        Utc::now() + ChronoDuration::hours(1),
    )
    .await
    .unwrap();
    let mut credentials = Vec::new();
    for scope in ["write", "read"] {
        let value = native::token();
        let token = tokens::create_token(
            &database.pool,
            owner.id,
            &session,
            scope,
            &hex::encode(Sha256::digest(value.as_bytes())),
            scope,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        credentials.push((value, token.id));
    }
    let (write_token, write_id) = &credentials[0];
    let (read_token, read_id) = &credentials[1];
    let storage = Server::start(
        &root.path().join("storage"),
        &database,
        "git-storage",
        "web,storage",
        owner.id,
    )
    .await;
    let web = Server::start(
        &root.path().join("web"),
        &database,
        "git-web",
        "web,ssh",
        owner.id,
    )
    .await;
    let cookie = format!("session_id={session}");
    let csrf = native::token();
    assert_eq!(
        db::users::sessions::get_or_create_csrf_token(
            &database.pool,
            &session,
            Some(owner.id),
            &csrf,
            3600
        )
        .await
        .unwrap(),
        Some(csrf.clone())
    );
    let client = Client::new();
    let created = api(
        &client,
        &web,
        (&cookie, &csrf),
        reqwest::Method::POST,
        "/api/repos",
        json!({"name":"project", "is_private":true}),
        StatusCode::CREATED,
    )
    .await;
    let repository = db::repositories::records::get_repository_by_id(
        &database.pool,
        created["id"].as_i64().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(repository.storage_node_id, Some(storage.node_id));
    assert!(!Path::new(&repository.disk_path).exists());
    let uuid = rpc::repository_uuid(&repository.disk_path).unwrap();
    let storage_path = root
        .path()
        .join("storage/repos")
        .join(format!("{uuid}.git"));
    assert!(storage_path.is_dir());
    let http_url = format!("{}/remote_owner/project.git", web.base);
    let ssh_url = format!(
        "ssh://git@127.0.0.1:{}/remote_owner/project.git",
        web.ssh_port
    );
    let work = root.path().join("client");
    std::fs::create_dir(&work).unwrap();
    success(
        git(
            &work,
            &["init", "-q", "-b", "main", "source"],
            None,
            &key_path,
        )
        .await,
    );
    std::fs::create_dir_all(work.join("source/.github/workflows")).unwrap();
    std::fs::write(work.join("source/.github/workflows/ci.yml"), "name: test\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo test\n").unwrap();
    std::fs::write(work.join("source/README.md"), "remote\n").unwrap();
    success(git(&work, &["-C", "source", "add", "."], None, &key_path).await);
    success(
        git(
            &work,
            &["-C", "source", "commit", "-qm", "Initial"],
            None,
            &key_path,
        )
        .await,
    );
    let initial = success(
        git(
            &work,
            &["-C", "source", "rev-parse", "HEAD"],
            None,
            &key_path,
        )
        .await,
    );
    let denied = git(
        &work,
        &["-C", "source", "push", &http_url, "main"],
        Some(read_token),
        &key_path,
    )
    .await;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("403"));
    success(
        git(
            &work,
            &["-C", "source", "push", &http_url, "main"],
            Some(write_token),
            &key_path,
        )
        .await,
    );
    for (url, destination, token) in [
        (&http_url, "http", Some(read_token.as_str())),
        (&ssh_url, "ssh", None),
    ] {
        success(git(&work, &["clone", "-q", url, destination], token, &key_path).await);
        assert_eq!(
            success(
                git(
                    &work,
                    &["-C", destination, "rev-parse", "HEAD"],
                    None,
                    &key_path
                )
                .await
            ),
            initial
        );
        assert_eq!(
            std::fs::read_to_string(work.join(destination).join("README.md")).unwrap(),
            "remote\n"
        );
    }
    success(
        git(
            &work,
            &["-C", "source", "checkout", "-qb", "feature"],
            None,
            &key_path,
        )
        .await,
    );
    success(
        git(
            &work,
            &["-C", "source", "commit", "--allow-empty", "-qm", "Feature"],
            None,
            &key_path,
        )
        .await,
    );
    success(
        git(
            &work,
            &["-C", "source", "push", &ssh_url, "feature"],
            None,
            &key_path,
        )
        .await,
    );
    let opened = api(
        &client,
        &web,
        (&cookie, &csrf),
        reqwest::Method::POST,
        "/api/repos/remote_owner/project/pulls",
        json!({"title":"Feature", "head_branch":"feature", "base_branch":"main"}),
        StatusCode::CREATED,
    )
    .await;
    let pull_id = opened["id"].as_i64().unwrap();
    for (url, token, message) in [
        (&http_url, Some(write_token.as_str()), "HTTP feature"),
        (&ssh_url, None, "SSH feature"),
    ] {
        success(
            git(
                &work,
                &["-C", "source", "commit", "--allow-empty", "-qm", message],
                None,
                &key_path,
            )
            .await,
        );
        let sha = success(
            git(
                &work,
                &["-C", "source", "rev-parse", "HEAD"],
                None,
                &key_path,
            )
            .await,
        );
        success(
            git(
                &work,
                &["-C", "source", "push", url, "feature"],
                token,
                &key_path,
            )
            .await,
        );
        let db::DbPool::Postgres(raw) = &database.pool else {
            unreachable!()
        };
        let head: String = sqlx::query_scalar("SELECT head_sha FROM pull_requests WHERE id=$1")
            .bind(pull_id)
            .fetch_one(raw)
            .await
            .unwrap();
        assert_eq!(head, sha);
        let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND head_sha=$2 AND status='queued' AND event='push' AND triggered_by_id=$3").bind(repository.id).bind(&sha).bind(owner.id).fetch_one(raw).await.unwrap();
        assert_eq!(
            runs, 1,
            "push must queue one CI run for the exact accepted commit"
        );
        let accepted: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ci_accepted_pushes WHERE repo_id=$1 AND sha=$2 AND actor_id=$3",
        )
        .bind(repository.id)
        .bind(&sha)
        .bind(owner.id)
        .fetch_one(raw)
        .await
        .unwrap();
        assert_eq!(accepted, 1);
        for (destination, read) in [("http", Some(read_token.as_str())), ("ssh", None)] {
            success(
                git(
                    &work,
                    &["-C", destination, "fetch", "-q", "origin"],
                    read,
                    &key_path,
                )
                .await,
            );
            assert_eq!(
                success(
                    git(
                        &work,
                        &["-C", destination, "rev-parse", "origin/feature"],
                        None,
                        &key_path
                    )
                    .await
                ),
                sha
            );
        }
    }
    db::repositories::protection::upsert_branch_protection_rule(
        &database.pool,
        BranchProtectionRuleUpsert {
            actor_id: owner.id,
            repo_id: repository.id,
            pattern: "main",
            require_pull_request: true,
            required_approving_reviews: 0,
            dismiss_stale_reviews: false,
            require_status_checks: false,
            required_status_checks: vec![],
            require_up_to_date_branch: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for (url, token) in [(&http_url, Some(write_token.as_str())), (&ssh_url, None)] {
        let denied = git(
            &work,
            &["-C", "source", "push", url, "feature:main"],
            token,
            &key_path,
        )
        .await;
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("pre-receive hook declined"));
    }
    assert_eq!(
        git2::Repository::open_bare(&storage_path)
            .unwrap()
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        initial
    );

    // The storage node applies the rules of the branch it holds: a rewritten `feature` is
    // refused over HTTP and SSH until its rule allows force pushes.
    success(
        git(
            &work,
            &["-C", "source", "checkout", "-q", "feature"],
            None,
            &key_path,
        )
        .await,
    );
    success(
        git(
            &work,
            &[
                "-C",
                "source",
                "commit",
                "--amend",
                "--allow-empty",
                "-qm",
                "Rewritten",
            ],
            None,
            &key_path,
        )
        .await,
    );
    let rewritten = success(
        git(
            &work,
            &["-C", "source", "rev-parse", "HEAD"],
            None,
            &key_path,
        )
        .await,
    )
    .trim()
    .to_owned();
    for allow_force_pushes in [false, true] {
        db::repositories::protection::upsert_branch_protection_rule(
            &database.pool,
            BranchProtectionRuleUpsert {
                actor_id: owner.id,
                repo_id: repository.id,
                pattern: "feature",
                require_pull_request: false,
                block_force_pushes: !allow_force_pushes,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        for (url, token) in [(&http_url, Some(write_token.as_str())), (&ssh_url, None)] {
            let pushed = git(
                &work,
                &["-C", "source", "push", "--force", url, "feature"],
                token,
                &key_path,
            )
            .await;
            if allow_force_pushes {
                success(pushed);
                break;
            }
            assert!(!pushed.status.success());
            assert!(String::from_utf8_lossy(&pushed.stderr).contains("pre-receive hook declined"));
        }
        let held = git2::Repository::open_bare(&storage_path)
            .unwrap()
            .find_reference("refs/heads/feature")
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        assert_eq!(held == rewritten, allow_force_pushes);
    }
    db::users::ssh_keys::delete_ssh_key(&database.pool, key.id)
        .await
        .unwrap();
    let denied = git(
        &work,
        &["-C", "source", "push", &ssh_url, "feature:new-branch"],
        None,
        &key_path,
    )
    .await;
    assert!(!denied.status.success());
    assert!(
        git2::Repository::open_bare(&storage_path)
            .unwrap()
            .find_reference("refs/heads/new-branch")
            .is_err(),
        "a revoked key must not create a branch"
    );

    // Bypass ingress to verify storage-side role, placement and credential checks.
    let caller_disk = root.path().join("rpc-caller");
    let caller = cluster::start(
        &database.pool,
        &caller_disk.join("state"),
        &caller_disk,
        &"web".parse().unwrap(),
        cluster::Settings::from_lookup(&|name| match name {
            "IROH_BIND" => Some("127.0.0.1:0".into()),
            "NODE_NAME" => Some("git-caller".into()),
            _ => None,
        })
        .unwrap()
        .unwrap(),
    )
    .await
    .unwrap();
    for credential in [
        ReceiveCredential::Token(*read_id),
        ReceiveCredential::SshKey(key.id),
    ] {
        let error = refused_receive(
            &caller.context,
            storage.node_id,
            uuid,
            credential,
            &owner.username,
        )
        .await;
        assert_eq!(
            error.kind,
            codeyard::backend::repositories::store::ErrorKind::Unavailable
        );
    }
    let db::DbPool::Postgres(raw) = &database.pool else {
        unreachable!()
    };
    sqlx::query("UPDATE nodes SET roles=ARRAY['actions-worker'] WHERE id=$1")
        .bind(caller.context.node_id)
        .execute(raw)
        .await
        .unwrap();
    let error = refused_receive(
        &caller.context,
        storage.node_id,
        uuid,
        ReceiveCredential::Token(*write_id),
        &owner.username,
    )
    .await;
    assert_eq!(
        error.message,
        "repository RPC GitSession requires a web or ssh node"
    );
    sqlx::query("UPDATE nodes SET roles=ARRAY['web'] WHERE id=$1")
        .bind(caller.context.node_id)
        .execute(raw)
        .await
        .unwrap();
    let elsewhere = db::repositories::records::create_repository(
        &database.pool,
        owner.id,
        "elsewhere",
        None,
        false,
        root.path()
            .join(format!("{}.git", uuid::Uuid::new_v4()))
            .to_str()
            .unwrap(),
    )
    .await
    .unwrap();
    let error = refused_receive(
        &caller.context,
        storage.node_id,
        rpc::repository_uuid(&elsewhere.disk_path).unwrap(),
        ReceiveCredential::Token(*write_id),
        &owner.username,
    )
    .await;
    assert_eq!(error.message, "repository is not stored on this node");
    sqlx::query("UPDATE nodes SET status='revoked' WHERE id=$1")
        .bind(caller.context.node_id)
        .execute(raw)
        .await
        .unwrap();
    let _ = refused_receive(
        &caller.context,
        storage.node_id,
        uuid,
        ReceiveCredential::Token(*write_id),
        &owner.username,
    )
    .await;
    caller.shutdown().await;
    let fresh = db::repositories::records::get_repository_by_id(&database.pool, repository.id)
        .await
        .unwrap()
        .unwrap();
    assert!(fresh.size_bytes > 0);
    assert_eq!(fresh.default_branch, "main");
    assert!(!Path::new(&repository.disk_path).exists());
}

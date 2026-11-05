//! Real SSH request/reply and protocol negotiation regressions.
use super::*;
use crate::rate_limit::RateLimiter;
use crate::server::SshServer;
use russh::client;
use russh::keys::PublicKeyOrCertificate;
use russh::keys::{PrivateKeyWithHashAlg, PublicKey};
use russh::ChannelMsg;
use std::time::Duration;

struct Client;
impl client::Handler for Client {
    type Error = russh::Error;
    async fn check_server_key(&mut self, _: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    pool: DbPool,
    key: Arc<PrivateKey>,
    address: SocketAddr,
    limiter: RateLimiter,
    stop: watch::Sender<bool>,
    running: tokio::task::JoinHandle<Result<()>>,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_router("alice", 51, None).await
    }
    async fn with_router(
        name: &str,
        seed: u8,
        authorization: Option<Arc<dyn crate::handler::RouterAuthorization>>,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        let pool = codeyard_db_core::init_db(&format!(
            "sqlite://{}",
            root.path().join("app.sqlite").display()
        ))
        .await
        .unwrap();
        codeyard_db_core::run_migrations(&pool).await.unwrap();
        let user = codeyard_db_identity::users::accounts::create_user(&pool, name, None)
            .await
            .unwrap();
        let key = Arc::new(PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32])));
        codeyard_db_identity::users::ssh_keys::add_ssh_key(
            &pool,
            user.id,
            "client",
            &key.public_key().to_openssh().unwrap(),
        )
        .await
        .unwrap();
        codeyard_repos_core::repositories::lifecycle::provision(
            (&pool, None),
            root.path(),
            codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(user.id),
            "repo",
            None,
            false,
            None,
        )
        .await
        .unwrap();
        let ci = Arc::new(CiRuns::new(
            pool.clone(),
            Default::default(),
            Arc::new(tokio::sync::Notify::new()),
            None,
            Arc::new(codeyard_notifications::broadcast::NotificationBroadcaster::new(&pool)),
        ));
        // Another test can take the probed port before the listener binds it; try a new one.
        let (limiter, address, stop, running) = 'start: {
            for _ in 0..5 {
                let mut server = SshServer::new(pool.clone(), None, ci.clone())
                    .await
                    .unwrap();
                if let Some(authorization) = &authorization {
                    server = server.with_router_authorization(authorization.clone());
                }
                let limiter = server.handler.rate_limiter.clone();
                let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let address = probe.local_addr().unwrap();
                drop(probe);
                let (stop, stopped) = watch::channel(false);
                let mut running = tokio::spawn(async move { server.run(address, stopped).await });
                tokio::select! {
                    result = &mut running => match result {
                        Ok(Err(error)) if error.to_string().contains("in use") => continue,
                        result => panic!("SSH listener exited during startup: {result:?}"),
                    },
                    ready = tokio::time::timeout(Duration::from_secs(30), async {
                        loop {
                            if TcpStream::connect(address).await.is_ok() {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }) => ready.expect("SSH listener did not start"),
                }
                break 'start (limiter, address, stop, running);
            }
            panic!("no free port for the SSH listener");
        };
        Self {
            _root: root,
            pool,
            key,
            address,
            limiter,
            stop,
            running,
        }
    }
    async fn connect(&self) -> client::Handle<Client> {
        client::connect(Arc::new(client::Config::default()), self.address, Client)
            .await
            .unwrap()
    }
    async fn finish(self) {
        self.stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(10), self.running)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.pool.close().await;
    }
}

#[tokio::test]
async fn unsupported_requests_reply_and_git_protocol_is_channel_local() {
    let f = Fixture::new().await;
    let mut client = f.connect().await;
    assert!(client
        .authenticate_publickey("git", PrivateKeyWithHashAlg::new(f.key.clone(), None))
        .await
        .unwrap()
        .success());
    for request in ["shell", "subsystem", "pty"] {
        let mut channel = client.channel_open_session().await.unwrap();
        match request {
            "shell" => channel.request_shell(true).await.unwrap(),
            "subsystem" => channel.request_subsystem(true, "sftp").await.unwrap(),
            _ => channel
                .request_pty(true, "xterm", 80, 24, 0, 0, &[])
                .await
                .unwrap(),
        }
        let (mut failure, mut exit) = (false, false);
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(message) = channel.wait().await {
                match message {
                    ChannelMsg::Failure => failure = true,
                    ChannelMsg::ExitStatus { exit_status } => {
                        assert_ne!(exit_status, 0);
                        exit = true;
                    }
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
        })
        .await
        .expect("unsupported request hung");
        assert!(failure && exit, "{request}");
    }
    let mut v2 = client.channel_open_session().await.unwrap();
    for (name, value, accepted) in [
        ("LANG", "C", false),
        ("GIT_PROTOCOL", "version=3", false),
        ("GIT_PROTOCOL", "version=2", true),
    ] {
        v2.set_env(true, name, value).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), v2.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(reply, ChannelMsg::Success) == accepted,
            "{reply:?}"
        );
    }
    v2.exec(true, "git-upload-pack 'alice/repo.git'")
        .await
        .unwrap();
    let mut output = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(message) = v2.wait().await {
            if let ChannelMsg::Data { data } = message {
                output.extend_from_slice(&data);
                if output.windows(9).any(|w| w == b"version 2") {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(output.windows(9).any(|w| w == b"version 2"));
    v2.eof().await.unwrap();
    v2.close().await.unwrap();
    let mut v0 = client.channel_open_session().await.unwrap();
    v0.exec(true, "git-upload-pack 'alice/repo.git'")
        .await
        .unwrap();
    let mut output = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(message) = v0.wait().await {
            if let ChannelMsg::Data { data } = message {
                output.extend_from_slice(&data);
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(!output.windows(9).any(|w| w == b"version 2"));
    v0.eof().await.unwrap();
    v0.close().await.unwrap();
    client
        .disconnect(russh::Disconnect::ByApplication, "done", "en")
        .await
        .unwrap();
    drop(client);
    f.finish().await;
}

#[tokio::test]
async fn database_authentication_errors_do_not_ban_the_client() {
    let f = Fixture::new().await;
    f.pool.close().await;
    for _ in 0..6 {
        let mut client = f.connect().await;
        let result = client
            .authenticate_publickey("git", PrivateKeyWithHashAlg::new(f.key.clone(), None))
            .await;
        assert!(result.is_err() || !result.unwrap().success());
        drop(client);
    }
    assert!(!f.limiter.is_blocked("127.0.0.1".parse().unwrap()).await);
    f.finish().await;
}

struct RouterAuthorization {
    key: PublicKey,
    enabled: Arc<std::sync::atomic::AtomicBool>,
}
impl crate::handler::RouterAuthorization for RouterAuthorization {
    fn authorize<'a>(
        &'a self,
        key: &'a PublicKey,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + 'a>> {
        Box::pin(async move {
            Ok(key == &self.key && self.enabled.load(std::sync::atomic::Ordering::SeqCst))
        })
    }
}
struct RouterDirectory {
    entries: Vec<(String, String, SocketAddr, String)>,
}
impl crate::router::Directory for RouterDirectory {
    fn resolve<'a>(
        &'a self,
        owner: Option<&'a str>,
        fingerprint: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<crate::router::Target>>> + Send + 'a>,
    > {
        Box::pin(async move {
            Ok(self
                .entries
                .iter()
                .find(|(name, key, _, _)| owner.map_or(key == fingerprint, |owner| owner == name))
                .map(|(_, _, address, pin)| crate::router::Target {
                    addresses: vec![*address],
                    pins: vec![pin.clone()],
                }))
        })
    }
}
async fn route_entry(name: &str, fixture: &Fixture) -> (String, String, SocketAddr, String) {
    let key = codeyard_db_cluster::node_config::get_cluster_ssh_host_key(&fixture.pool)
        .await
        .unwrap()
        .unwrap();
    let key = russh::keys::decode_secret_key(&key, None).unwrap();
    let pin = key
        .public_key()
        .fingerprint(russh::keys::HashAlg::Sha256)
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let fingerprint = codeyard_db_identity::users::ssh_keys::calculate_fingerprint(
        &fixture.key.public_key().to_openssh().unwrap(),
    )
    .unwrap();
    (name.into(), fingerprint, fixture.address, pin)
}
async fn routed_client(address: SocketAddr, key: Arc<PrivateKey>) -> client::Handle<Client> {
    let mut client = client::connect(Arc::new(client::Config::default()), address, Client)
        .await
        .unwrap();
    assert!(client
        .authenticate_publickey("git", PrivateKeyWithHashAlg::new(key, None))
        .await
        .unwrap()
        .success());
    client
}
async fn advertisement(client: &client::Handle<Client>, owner: &str, available: bool) {
    let mut channel = client.channel_open_session().await.unwrap();
    // A client cannot replace its signed identity with an environment variable.
    channel
        .set_env(true, "CODEYARD_SSH_KEY", "ssh-ed25519 forged")
        .await
        .unwrap();
    assert!(matches!(channel.wait().await, Some(ChannelMsg::Failure)));
    channel
        .set_env(true, "GIT_PROTOCOL", "version=2")
        .await
        .unwrap();
    assert!(matches!(channel.wait().await, Some(ChannelMsg::Success)));
    channel
        .exec(true, format!("git-upload-pack '{owner}/repo.git'"))
        .await
        .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => {
                    bytes.extend_from_slice(&data);
                    if available && bytes.windows(9).any(|bytes| bytes == b"version 2") {
                        break;
                    }
                }
                ChannelMsg::Close => break,
                _ => {}
            }
        }
    })
    .await
    .expect("routed SSH command hung");
    assert_eq!(
        bytes.windows(9).any(|bytes| bytes == b"version 2"),
        available
    );
    let _ = channel.eof().await;
    let _ = channel.close().await;
}
#[tokio::test]
async fn public_ssh_router_pins_native_cells_preserves_identity_and_rechecks_router_revocation() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let root = tempfile::tempdir().unwrap();
    let identity = PrivateKey::from(Ed25519Keypair::from_seed(&[52; 32]));
    let enabled = Arc::new(AtomicBool::new(true));
    let authorization = Arc::new(RouterAuthorization {
        key: identity.public_key().clone(),
        enabled: enabled.clone(),
    });
    let alice = Fixture::with_router("alice", 53, Some(authorization.clone())).await;
    let bob = Fixture::with_router("bob", 54, Some(authorization)).await;
    let directory = Arc::new(RouterDirectory {
        entries: vec![
            route_entry("alice", &alice).await,
            route_entry("bob", &bob).await,
        ],
    });
    let handler = crate::router::Handler::new(directory, identity);
    let mut router =
        SshServer::with_handler(root.path().join("host_key").to_str().unwrap(), handler).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = watch::channel(false);
    let task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        router.run_listener(listener, stopped).await
    }));
    let first = routed_client(address, alice.key.clone()).await;
    let second = routed_client(address, bob.key.clone()).await;
    advertisement(&first, "alice", true).await;
    advertisement(&second, "bob", true).await;
    // The receiving cell authorizes the original key, never the router's account.
    advertisement(&first, "bob", false).await;
    enabled.store(false, Ordering::SeqCst);
    advertisement(&first, "alice", false).await;
    enabled.store(true, Ordering::SeqCst);
    // A stopped cell leaves other owners reachable through the same public listener.
    advertisement(&first, "alice", true).await;
    let _ = second
        .disconnect(russh::Disconnect::ByApplication, "done", "en")
        .await;
    drop(second);
    bob.finish().await;
    advertisement(&first, "bob", false).await;
    advertisement(&first, "alice", true).await;
    let _ = first
        .disconnect(russh::Disconnect::ByApplication, "done", "en")
        .await;
    drop(first);
    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    alice.finish().await;
}

struct ForeignHome {
    user: codeyard_db_core::models::User,
    fingerprint: String,
    live: Arc<std::sync::atomic::AtomicBool>,
}
impl codeyard_db_core::cells::authorization::Home for ForeignHome {
    fn user<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<codeyard_db_core::models::User>>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            Ok(
                (self.live.load(std::sync::atomic::Ordering::SeqCst) && self.user.username == name)
                    .then(|| self.user.clone()),
            )
        })
    }
    fn authorize<'a>(
        &'a self,
        proof: &'a codeyard_db_core::cells::authorization::Proof,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Option<codeyard_db_core::cells::authorization::Answer>>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            use codeyard_db_core::cells::authorization::{Answer, Authority, Proof};
            Ok((self.live.load(std::sync::atomic::Ordering::SeqCst)
                && matches!(proof,Proof::Ssh {fingerprint} if fingerprint==&self.fingerprint))
            .then(|| Answer {
                home: codeyard_support::cells::Prefix::new(1),
                user: self.user.clone(),
                credential_id: Some(self.user.id + 7),
                write: true,
                expires_at: None,
                authority: Authority::Ssh,
            }))
        })
    }
}
async fn git(arguments: &[&str], ssh: Option<&str>) -> std::process::Output {
    let mut command = tokio::process::Command::new("git");
    command
        .args(arguments)
        .kill_on_drop(true)
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(ssh) = ssh {
        command.env("GIT_SSH_COMMAND", ssh);
    }
    tokio::time::timeout(Duration::from_secs(45), command.output())
        .await
        .unwrap()
        .unwrap()
}
#[tokio::test]
async fn foreign_collaborator_pushes_and_clones_through_public_ssh_then_loses_access() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let root = tempfile::tempdir().unwrap();
    let identity = PrivateKey::from(Ed25519Keypair::from_seed(&[61; 32]));
    let authorization = Arc::new(RouterAuthorization {
        key: identity.public_key().clone(),
        enabled: Arc::new(AtomicBool::new(true)),
    });
    let alice = Fixture::with_router("alice", 62, Some(authorization.clone())).await;
    let bob = Fixture::with_router("bob", 63, Some(authorization)).await;
    let fingerprint = codeyard_db_identity::users::ssh_keys::calculate_fingerprint(
        &bob.key.public_key().to_openssh().unwrap(),
    )
    .unwrap();
    let mut remote = codeyard_db_identity::users::accounts::get_user_by_username(&bob.pool, "bob")
        .await
        .unwrap()
        .unwrap();
    remote.id = *codeyard_support::cells::Prefix::new(1).ids().start();
    remote.home_cell = 1;
    remote.email = None;
    remote.password_hash = None;
    remote.avatar_file_id = None;
    remote.is_admin = false;
    remote.email_verified = false;
    let live = Arc::new(AtomicBool::new(true));
    alice.pool.set_foreign_authorization(Arc::new(ForeignHome {
        user: remote.clone(),
        fingerprint: fingerprint.clone(),
        live: live.clone(),
    }));
    // The unsigned key hint grants no repository access. The signed session revalidates it.
    codeyard_db_core::cells::authorization::authorize(
        &alice.pool,
        &codeyard_db_core::cells::authorization::Proof::Ssh { fingerprint },
    )
    .await
    .unwrap()
    .unwrap();
    let repo =
        codeyard_db_repos::repositories::records::get_repository(&alice.pool, "alice", "repo")
            .await
            .unwrap()
            .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        &alice.pool,
        remote.id,
        repo.id,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let directory = Arc::new(RouterDirectory {
        entries: vec![
            route_entry("alice", &alice).await,
            route_entry("bob", &bob).await,
        ],
    });
    let handler = crate::router::Handler::new(directory, identity);
    let mut router =
        SshServer::with_handler(root.path().join("host").to_str().unwrap(), handler).unwrap();
    let host = router.config.keys[0].public_key().to_openssh().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = watch::channel(false);
    let task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        router.run_listener(listener, stopped).await
    }));
    let key = root.path().join("key");
    std::fs::write(
        &key,
        bob.key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let known = root.path().join("known_hosts");
    std::fs::write(&known, format!("[127.0.0.1]:{} {host}\n", address.port())).unwrap();
    let quote =
        |path: &std::path::Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
    let ssh=format!("ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile={} -i {} -p {}",quote(&known),quote(&key),address.port());
    let work = root.path().join("work");
    let work = work.to_str().unwrap();
    assert!(git(&["init", "--initial-branch=main", work], None)
        .await
        .status
        .success());
    assert!(git(
        &[
            "-C",
            work,
            "-c",
            "user.name=bob",
            "-c",
            "user.email=bob@example.test",
            "commit",
            "--allow-empty",
            "-m",
            "first"
        ],
        None
    )
    .await
    .status
    .success());
    let url = format!("ssh://git@127.0.0.1:{}/alice/repo.git", address.port());
    let pushed = git(
        &["-C", work, "push", &url, "HEAD:refs/heads/main"],
        Some(&ssh),
    )
    .await;
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    let cloned = root.path().join("clone");
    let cloned = git(
        &["clone", "--branch", "main", &url, cloned.to_str().unwrap()],
        Some(&ssh),
    )
    .await;
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    let before = git2::Repository::open_bare(&repo.disk_path)
        .unwrap()
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap();
    let codeyard_db_core::DbPool::Sqlite(raw) = &alice.pool else {
        unreachable!()
    };
    sqlx::query("DELETE FROM repo_access WHERE repo_id=$1 AND user_id=$2")
        .bind(repo.id)
        .bind(remote.id)
        .execute(raw)
        .await
        .unwrap();
    assert!(git(
        &[
            "-C",
            work,
            "-c",
            "user.name=bob",
            "-c",
            "user.email=bob@example.test",
            "commit",
            "--allow-empty",
            "-m",
            "second"
        ],
        None
    )
    .await
    .status
    .success());
    assert!(!git(
        &["-C", work, "push", &url, "HEAD:refs/heads/main"],
        Some(&ssh)
    )
    .await
    .status
    .success());
    codeyard_db_repos::repositories::access::grant_repo_access(
        &alice.pool,
        remote.id,
        repo.id,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    live.store(false, Ordering::SeqCst);
    sqlx::query("UPDATE foreign_credentials SET expires_at=validated_at")
        .execute(raw)
        .await
        .unwrap();
    assert!(!git(
        &["-C", work, "push", &url, "HEAD:refs/heads/main"],
        Some(&ssh)
    )
    .await
    .status
    .success());
    assert_eq!(
        git2::Repository::open_bare(&repo.disk_path)
            .unwrap()
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap(),
        before
    );
    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    alice.finish().await;
    bob.finish().await;
}

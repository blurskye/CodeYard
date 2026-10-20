//! Real SSH request/reply and protocol negotiation regressions.
use super::*;
use crate::backend::db;
use russh::{
    client,
    keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate},
    ChannelMsg,
};
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
        let root = tempfile::tempdir().unwrap();
        let pool = db::init_db(&format!(
            "sqlite://{}",
            root.path().join("app.sqlite").display()
        ))
        .await
        .unwrap();
        db::run_migrations(&pool).await.unwrap();
        let user = db::users::accounts::create_user(&pool, "alice", None)
            .await
            .unwrap();
        let key = Arc::new(PrivateKey::from(Ed25519Keypair::from_seed(&[51; 32])));
        db::users::ssh_keys::add_ssh_key(
            &pool,
            user.id,
            "client",
            &key.public_key().to_openssh().unwrap(),
        )
        .await
        .unwrap();
        let path = root.path().join("repo.git");
        git2::Repository::init_bare(&path).unwrap();
        db::repositories::records::create_repository(
            &pool,
            user.id,
            "repo",
            None,
            false,
            path.to_str().unwrap(),
        )
        .await
        .unwrap();
        let ci = Arc::new(CiRuns::new(
            pool.clone(),
            Default::default(),
            Arc::new(tokio::sync::Notify::new()),
            None,
            Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                &pool,
            )),
        ));
        // Another test can take the probed port before the listener binds it; try a new one.
        let (limiter, address, stop, running) = 'start: {
            for _ in 0..5 {
                let mut server = SshServer::new(pool.clone(), None, ci.clone())
                    .await
                    .unwrap();
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

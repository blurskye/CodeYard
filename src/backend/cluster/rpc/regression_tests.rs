use super::*;
use crate::backend::{
    ci::CiConfig,
    cluster::{IrohDialer, NodeKey, Relay},
    db::{self, DbPool},
    repositories::{
        git_sessions::ReceiveCredential,
        store::{GitService, GitSession, RepoStore},
    },
};
use std::{path::Path, sync::Arc, time::Duration};

async fn context(path: &Path) -> NodeContext {
    let pool = db::init_db(&format!("sqlite://{}", path.join("test.db").display()))
        .await
        .unwrap();
    db::run_migrations(&pool).await.unwrap();
    let key = NodeKey::generate();
    let node = db::cluster::register_own_node(
        &pool,
        &key.public_hex(),
        "storage",
        &["web".into(), "storage".into()],
        "test",
    )
    .await
    .unwrap();
    NodeContext {
        pool: Some(pool),
        iroh: IrohDialer::new(Relay::Off, key.clone()),
        key,
        node_id: node.id,
        repos_path: path.to_owned(),
        peers: Default::default(),
        unreachable: Default::default(),
        ci_replicas: Default::default(),
    }
}

struct HeaderReader {
    reader: tokio::io::DuplexStream,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
}
impl tokio::io::AsyncRead for HeaderReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(());
        }
        std::pin::Pin::new(&mut self.reader).poll_read(cx, buffer)
    }
}

#[tokio::test]
async fn rpc_and_git_admission_deadlines_include_database_waits() {
    let directory = tempfile::tempdir().unwrap();
    let mut context = context(directory.path()).await;
    let raw = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(600))
        .connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
    context.pool = Some(raw.clone().into());
    let held = raw.acquire().await.unwrap();
    tokio::time::pause();
    let request = Request {
        repository: uuid::Uuid::new_v4(),
        operation: Operation::Refs,
        actor: None,
        ci_run: None,
    };
    let start = tokio::time::Instant::now();
    assert!(context
        .call(context.node_id + 1, request, b"")
        .await
        .is_err());
    assert!((TIMEOUT..=TIMEOUT + Duration::from_millis(2)).contains(&start.elapsed()));
    let request = Request {
        repository: uuid::Uuid::new_v4(),
        operation: Operation::GitSession(GitSession::new(GitService::UploadPack, true)),
        actor: None,
        ci_run: None,
    };
    let start = tokio::time::Instant::now();
    assert!(context
        .git_session(
            context.node_id + 1,
            request,
            crate::backend::repositories::store::GitStreams {
                input: &mut tokio::io::empty(),
                output: &mut tokio::io::sink(),
                errors: &mut tokio::io::sink(),
            }
        )
        .await
        .is_err());
    assert!((TIMEOUT..=TIMEOUT + Duration::from_millis(2)).contains(&start.elapsed()));
    drop(held);
}

#[tokio::test]
async fn editor_and_streamed_admission_release_their_slots_at_the_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let mut context = context(directory.path()).await;
    let raw = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(600))
        .connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
    context.pool = Some(raw.clone().into());
    let handler = RpcHandler::new(context.clone());
    let (ready, read_header) = tokio::sync::oneshot::channel();
    let (mut client, reader) = tokio::io::duplex(4096);
    let link = Link::tcp(
        context.key.public_key(),
        transport::ALPN_RPC,
        HeaderReader {
            reader,
            ready: Some(ready),
        },
        tokio::io::sink(),
    );
    let serving = handler.clone();
    let task = tokio::spawn(async move { serving.serve_link(link).await });
    read_header.await.unwrap();
    let held = raw.acquire().await.unwrap();
    let request = Request {
        repository: uuid::Uuid::new_v4(),
        actor: Some(1),
        ci_run: None,
        operation: Operation::SaveFile {
            write: crate::backend::repositories::store::FileWrite {
                branch: "main".into(),
                path: "file.txt".into(),
                expected_revision: "unborn".into(),
                message: "save".into(),
                creating: true,
                deleting: false,
            },
            author: "Test".into(),
            email: "test@example.invalid".into(),
        },
    };
    write_frame(
        &mut client,
        &serde_json::to_vec(&request).unwrap(),
        MAX_JSON,
    )
    .await
    .unwrap();
    tokio::time::pause();
    let start = tokio::time::Instant::now();
    assert!(task.await.unwrap().is_err());
    assert!(start.elapsed() <= TIMEOUT + Duration::from_secs(1));
    assert_eq!(handler.slots.available_permits(), 16);
    let request = Request {
        repository: uuid::Uuid::new_v4(),
        operation: Operation::GitSession(GitSession::new(GitService::UploadPack, true)),
        actor: None,
        ci_run: None,
    };
    let link = Link::tcp(
        context.key.public_key(),
        transport::ALPN_RPC,
        tokio::io::empty(),
        tokio::io::sink(),
    );
    let start = tokio::time::Instant::now();
    handler
        .serve_session(
            request,
            link,
            tokio::time::Instant::now() + TIMEOUT,
            (
                handler.session_headers.clone().try_acquire_owned().unwrap(),
                handler.header_slot(context.key.public_key()).unwrap(),
            ),
        )
        .await
        .unwrap();
    assert!((TIMEOUT..=TIMEOUT + Duration::from_millis(2)).contains(&start.elapsed()));
    drop(held);
}

#[tokio::test]
async fn streamed_admission_retains_bounded_permits_until_it_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let handler = RpcHandler::new(context.clone());
    let peer = context.key.public_key();
    let global = handler.session_headers.clone().try_acquire_owned().unwrap();
    let local = handler.header_slot(peer).unwrap();
    let request = Request {
        repository: uuid::Uuid::new_v4(),
        operation: Operation::GitSession(GitSession::new(GitService::UploadPack, true)),
        actor: None,
        ci_run: None,
    };
    let link = Link::tcp(
        peer,
        transport::ALPN_RPC,
        tokio::io::empty(),
        tokio::io::sink(),
    );
    let future = handler.serve_session(
        request,
        link,
        tokio::time::Instant::now() + TIMEOUT,
        (global, local),
    );
    assert_eq!(handler.session_headers.available_permits(), 15);
    let other: Vec<_> = (0..3).map(|_| handler.header_slot(peer).unwrap()).collect();
    assert!(handler.header_slot(peer).is_err());
    // Queued headers do not hold buffered-RPC slots, and cancellation frees them.
    assert_eq!(handler.slots.available_permits(), 16);
    drop(future);
    drop(other);
    assert_eq!(handler.session_headers.available_permits(), 16);
    assert!(handler.header_slot(peer).is_ok());
}

#[tokio::test]
async fn rpc_header_admission_refuses_excess_connections_and_limits_each_peer() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let handler = RpcHandler::new(context.clone());
    let peer = context.key.public_key();
    let permits: Vec<_> = (0..4).map(|_| handler.header_slot(peer).unwrap()).collect();
    assert!(handler.header_slot(peer).is_err());
    assert!(handler
        .header_slot(NodeKey::generate().public_key())
        .is_ok());
    drop(permits);
    assert!(handler.header_slot(peer).is_ok());
    let _full = handler.slots.acquire_many(16).await.unwrap();
    let (_client, reader) = tokio::io::duplex(1024);
    let link = Link::tcp(peer, transport::ALPN_RPC, reader, tokio::io::sink());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), handler.serve_link(link))
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn storage_git_admission_rechecks_live_tokens_keys_and_grants() {
    use base64::Engine;
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let pool = context.database().unwrap();
    let owner = db::users::accounts::create_user(pool, "owner", None)
        .await
        .unwrap();
    let actor = db::users::accounts::create_user(pool, "reader", None)
        .await
        .unwrap();
    let store = RepoStore::create(directory.path(), "main", None)
        .await
        .unwrap();
    let repo = db::repositories::records::create_repository(
        pool,
        owner.id,
        "private",
        None,
        true,
        store.location(),
    )
    .await
    .unwrap();
    db::cluster::place_repository(pool, repo.id, context.node_id)
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(
        pool,
        actor.id,
        repo.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let session_id = "a".repeat(64);
    db::users::sessions::create_session(
        pool,
        &session_id,
        actor.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let token = db::users::tokens::create_token(
        pool,
        actor.id,
        &session_id,
        "read",
        &"b".repeat(64),
        "read",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let mut wire = Vec::from(&b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..]);
    wire.extend_from_slice(&[7; 32]);
    let public = format!(
        "ssh-ed25519 {}",
        base64::engine::general_purpose::STANDARD.encode(wire)
    );
    let key = db::users::ssh_keys::add_ssh_key(pool, actor.id, "reader", &public)
        .await
        .unwrap();
    let request = |credential| {
        let mut session = GitSession::new(GitService::UploadPack, true);
        session.credential = Some(credential);
        Request {
            repository: repository_uuid(store.location()).unwrap(),
            operation: Operation::GitSession(session),
            actor: None,
            ci_run: None,
        }
    };
    let handler = RpcHandler::new(context.clone());
    for credential in [
        ReceiveCredential::Token(token.id),
        ReceiveCredential::SshKey(key.id),
    ] {
        handler
            .authorize_request(&request(credential), context.key.public_key())
            .await
            .unwrap();
    }
    db::repositories::access::revoke_repo_access(pool, actor.id, repo.id)
        .await
        .unwrap();
    for credential in [
        ReceiveCredential::Token(token.id),
        ReceiveCredential::SshKey(key.id),
    ] {
        assert!(handler
            .authorize_request(&request(credential), context.key.public_key())
            .await
            .is_err());
    }
    db::repositories::access::grant_repo_access(
        pool,
        actor.id,
        repo.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    db::users::tokens::delete_token(pool, actor.id, token.id)
        .await
        .unwrap();
    let DbPool::Sqlite(raw) = pool else {
        unreachable!()
    };
    sqlx::query("DELETE FROM ssh_keys WHERE id=?")
        .bind(key.id)
        .execute(raw)
        .await
        .unwrap();
    for credential in [
        ReceiveCredential::Token(token.id),
        ReceiveCredential::SshKey(key.id),
    ] {
        assert!(handler
            .authorize_request(&request(credential), context.key.public_key())
            .await
            .is_err());
    }
}

#[tokio::test]
async fn buffered_requests_are_drained_and_new_requests_are_refused() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let handler = RpcHandler::new(context.clone());
    let (ready, reading) = tokio::sync::oneshot::channel();
    let (mut client, reader) = tokio::io::duplex(4096);
    let link = Link::tcp(
        context.key.public_key(),
        transport::ALPN_RPC,
        HeaderReader {
            reader,
            ready: Some(ready),
        },
        tokio::io::sink(),
    );
    let serving = handler.clone();
    let task = tokio::spawn(async move { serving.serve_link(link).await });
    reading.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), handler.sessions.drain())
            .await
            .is_err()
    );
    // An admitted buffered request remains tracked until its response is complete.
    let header = serde_json::to_vec(&Request {
        repository: uuid::Uuid::new_v4(),
        operation: Operation::Refs,
        actor: None,
        ci_run: None,
    })
    .unwrap();
    write_frame(&mut client, &header, MAX_JSON).await.unwrap();
    write_frame(&mut client, b"", MAX_BYTES).await.unwrap();
    task.await.unwrap().unwrap();
    handler.sessions.drain().await;
    let (ready, mut reading) = tokio::sync::oneshot::channel();
    let (_, reader) = tokio::io::duplex(4096);
    handler
        .serve_link(Link::tcp(
            context.key.public_key(),
            transport::ALPN_RPC,
            HeaderReader {
                reader,
                ready: Some(ready),
            },
            tokio::io::sink(),
        ))
        .await
        .unwrap();
    assert!(reading.try_recv().is_err());
}

#[tokio::test]
async fn connected_but_stalled_primary_leaves_time_for_buffered_and_streamed_failover() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let pool = context.database().unwrap();
    let key = NodeKey::generate();
    let primary = db::cluster::register_own_node(
        pool,
        &key.public_hex(),
        "primary",
        &["storage".into()],
        "test",
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = crate::backend::cluster::NodeAddress {
        tcp: vec![listener.local_addr().unwrap()],
        ..Default::default()
    };
    db::cluster::record_node_heartbeat(
        pool,
        &key.public_hex(),
        "test",
        &serde_json::to_string(&address).unwrap(),
        Some(100),
    )
    .await
    .unwrap();
    db::cluster::record_node_heartbeat(pool, &context.key.public_hex(), "test", "{}", Some(100))
        .await
        .unwrap();
    let owner = db::users::accounts::create_user(pool, "owner", None)
        .await
        .unwrap();
    let uuid = uuid::Uuid::new_v4();
    let path = crate::backend::cluster::replicas::copy_path(directory.path(), uuid);
    let repo = db::repositories::records::create_repository(
        pool,
        owner.id,
        "repo",
        None,
        false,
        &format!("/repos/{uuid}.git"),
    )
    .await
    .unwrap();
    db::cluster::place_repository(pool, repo.id, primary.id)
        .await
        .unwrap();
    let DbPool::Sqlite(raw) = pool else {
        unreachable!()
    };
    sqlx::query(
        "INSERT INTO repository_replicas(repository_id,node_id,applied_version) VALUES(?,?,0)",
    )
    .bind(repo.id)
    .bind(context.node_id)
    .execute(raw)
    .await
    .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    git2::Repository::init_bare(&path).unwrap();
    let acceptor =
        tokio_rustls::TlsAcceptor::from(crate::backend::cluster::tls::server_config(&key).unwrap());
    let (ready, mut waiting) = tokio::sync::mpsc::channel(2);
    let server = tokio::spawn(async move {
        let mut links = tokio::task::JoinSet::new();
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            let ready = ready.clone();
            links.spawn(async move {
                let mut stream = acceptor.accept(socket).await.unwrap();
                read_frame(&mut stream, MAX_JSON).await.unwrap();
                ready.send(()).await.unwrap();
                std::future::pending::<()>().await;
            });
        }
    });
    for streamed in [false, true] {
        let request = Request {
            repository: uuid,
            operation: if streamed {
                Operation::GitSession(GitSession::new(GitService::UploadPack, true))
            } else {
                Operation::Refs
            },
            actor: None,
            ci_run: None,
        };
        let caller = context.clone();
        let task = tokio::spawn(async move {
            if streamed {
                caller
                    .git_session(
                        primary.id,
                        request,
                        crate::backend::repositories::store::GitStreams {
                            input: &mut std::io::Cursor::new(b"0000"),
                            output: &mut tokio::io::sink(),
                            errors: &mut tokio::io::sink(),
                        },
                    )
                    .await
                    .map(|(_, status)| status.success())
                    .unwrap()
            } else {
                caller
                    .call(primary.id, request, b"")
                    .await
                    .unwrap()
                    .0
                    .result
                    .is_ok()
            }
        });
        waiting.recv().await.unwrap();
        tokio::time::pause();
        tokio::time::advance(TIMEOUT / 2 + Duration::from_millis(1)).await;
        tokio::time::resume();
        assert!(tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap());
    }
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn stopping_workers_signals_and_joins_the_started_owner() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let pool = context.database().unwrap().clone();
    let config = Arc::new(CiConfig::default());
    let notifications = Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
        &pool,
    ));
    let queue = crate::backend::ci::queue::RunQueue::Database {
        pool: pool.clone(),
        config,
        secrets: None,
        context: Some(context.clone()),
        notifications,
    };
    let workers = crate::backend::ci::CiWorkers::new(
        queue,
        Arc::new(CiConfig::default()),
        false,
        directory.path(),
    );
    workers.startup().await.unwrap();
    workers.request_stop();
    workers.shutdown().await.unwrap();
    // A stopped local owner has joined its tasks before the database is closed.
    pool.close().await;
}

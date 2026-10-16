//! Database-free workers reach the real core queue over encrypted loopback iroh.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
#[path = "support/native_database.rs"]
mod native;
#[path = "support/pull_requests.rs"]
mod pulls;
#[path = "support/rpc_operations.rs"]
mod rpc_operations;

use codeyard::backend::{
    ci::{
        queue::{LogLine, RunQueue},
        CiConfig, CiExecutionMode, CiWorkers,
    },
    cluster::{self, rpc, NodeContext},
    crypto::SecretEncryptor,
    db,
    notifications::{current_event, NotificationBroadcaster, NotificationEvent},
    repositories::store::{ErrorKind, GitService, GitSession, RepoStore},
};
use iroh::protocol::ProtocolHandler;
use serde_json::json;
use std::{
    collections::BTreeSet,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;
use uuid::Uuid;

fn line(text: &str) -> LogLine {
    LogLine {
        stage: "ci.yml".into(),
        line: text.into(),
        level: "info".into(),
    }
}

async fn client(key: &cluster::NodeKey) -> NodeContext {
    NodeContext {
        pool: None,
        iroh: cluster::IrohDialer::with_endpoint(
            cluster::Relay::Off,
            key.clone(),
            cluster::bind_client(&cluster::Relay::Off, key)
                .await
                .unwrap(),
        ),
        key: key.clone(),
        node_id: 0,
        repos_path: Default::default(),
        peers: Default::default(),
        unreachable: Default::default(),
    }
}

async fn enqueue(
    pool: &db::DbPool,
    repo: i64,
    owner: i64,
    sha: &str,
    blob: &str,
) -> db::models::CiRun {
    db::ci::enqueue_ci_run(
        pool,
        db::ci::NewCiRun {
            repo_id: repo,
            triggered_by_id: owner,
            pipeline: "ci",
            event: "push",
            pull_request_id: None,
            branch: "main",
            head_sha: sha,
            workflow_path: ".github/workflows/ci.yml",
            workflow_blobs: &json!({".github/workflows/ci.yml": blob}),
            workflow_inputs: None,
            environment: None,
        },
        &[owner],
    )
    .await
    .unwrap()
}

async fn stream_request(
    connection: &iroh::endpoint::Connection,
    request: serde_json::Value,
) -> serde_json::Value {
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    rpc::write_frame(
        &mut send,
        &serde_json::to_vec(&request).unwrap(),
        rpc::MAX_JSON,
    )
    .await
    .unwrap();
    send.finish().unwrap();
    serde_json::from_slice(&rpc::read_frame(&mut recv, rpc::MAX_JSON).await.unwrap()).unwrap()
}

async fn repository_refused_with_claim(
    connection: &iroh::endpoint::Connection,
    repository: Uuid,
    operation: rpc::Operation,
    ci_run: Option<rpc::CiRunAccess>,
    message: &str,
) {
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    let streaming = matches!(
        &operation,
        rpc::Operation::GitSession(_) | rpc::Operation::ReplicaPack { .. }
    );
    rpc::write_frame(
        &mut send,
        &serde_json::to_vec(&rpc::Request {
            repository,
            operation,
            actor: None,
            ci_run,
        })
        .unwrap(),
        rpc::MAX_JSON,
    )
    .await
    .unwrap();
    if !streaming {
        rpc::write_frame(&mut send, b"", rpc::MAX_BYTES)
            .await
            .unwrap();
        send.finish().unwrap();
    }
    let reply: rpc::Reply =
        serde_json::from_slice(&rpc::read_frame(&mut recv, rpc::MAX_JSON).await.unwrap()).unwrap();
    assert!(rpc::read_frame(&mut recv, rpc::MAX_BYTES)
        .await
        .unwrap()
        .is_empty());
    match reply.result {
        Err(rpc::Failure::Repository(error)) => {
            assert_eq!(error.kind, ErrorKind::Invalid);
            assert!(error.message.contains(message), "{error}");
        }
        result => panic!("expected repository RPC refusal, got {result:?}"),
    }
}

async fn repository_refused(
    connection: &iroh::endpoint::Connection,
    repository: Uuid,
    operation: rpc::Operation,
    message: &str,
) {
    repository_refused_with_claim(connection, repository, operation, None, message).await;
}

#[tokio::test]
async fn database_free_worker_claims_runs_and_core_enforces_membership_and_ownership() {
    const CHILD: &str = "GITSERV_REMOTE_CI_WORKSPACE_UMASK_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("database_free_worker_claims_runs_and_core_enforces_membership_and_ownership")
            .arg("--nocapture")
            .env(CHILD, "1");
        unsafe {
            command.pre_exec(|| {
                libc::umask(0o022);
                Ok(())
            });
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let core_key = cluster::NodeKey::generate();
    let core_node = db::cluster::register_own_node(
        pool,
        &core_key.public_hex(),
        "ci-core",
        &["storage".into()],
        "test",
    )
    .await
    .unwrap();
    let endpoint = cluster::bind(
        &cluster::Listen {
            bind: "127.0.0.1:0".parse().unwrap(),
            relay: cluster::Relay::Off,
            advertise: vec![],
        },
        &core_key,
        pool.clone(),
    )
    .await
    .unwrap();
    let context = NodeContext {
        pool: Some(pool.clone()),
        iroh: cluster::IrohDialer::with_endpoint(
            cluster::Relay::Off,
            core_key.clone(),
            endpoint.clone(),
        ),
        key: core_key.clone(),
        node_id: core_node.id,
        repos_path: root.path().to_owned(),
        peers: Default::default(),
        unreachable: Default::default(),
    };
    let owner = db::users::accounts::create_user(pool, "ci_owner", None)
        .await
        .unwrap();
    let config = Arc::new(CiConfig {
        trusted_actor_ids: BTreeSet::from([owner.id]),
        ..CiConfig::default()
    });
    let encryptor = Arc::new(SecretEncryptor::new(&[42; 32]));
    let notifications = Arc::new(NotificationBroadcaster::new(pool));
    let mut subscriber = notifications.subscribe();
    let session = Uuid::new_v4().simple().to_string().repeat(2);
    db::users::sessions::create_session(
        pool,
        &session,
        owner.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let other_user = db::users::accounts::create_user(pool, "ci_other", None)
        .await
        .unwrap();
    let queue = RunQueue::Database {
        pool: pool.clone(),
        config,
        secrets: Some(encryptor.clone()),
        context: Some(context.clone()),
        notifications,
    };
    let router = cluster::router(endpoint.clone(), cluster::PairingHandler::new(pool.clone()))
        .accept(cluster::ALPN_RPC, rpc::RpcHandler::new(context.clone()))
        .accept(
            cluster::ALPN_EVENTS,
            cluster::ci::CiHandler::new(context.clone(), queue),
        )
        .spawn();
    cluster::wait_for_address(&endpoint).await;
    db::cluster::record_node_heartbeat(
        pool,
        &core_key.public_hex(),
        "test",
        &serde_json::to_string(&cluster::NodeAddress::of(&endpoint.addr())).unwrap(),
        Some(1),
    )
    .await
    .unwrap();
    let uuid = Uuid::new_v4();
    let path = root.path().join(format!("{uuid}.git"));
    let store = RepoStore::from_disk_path(path.to_str().unwrap());
    store.initialize(None, "main").await.unwrap();
    let repo = db::repositories::records::create_repository(
        pool,
        owner.id,
        "project",
        None,
        true,
        path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let workflow = b"name: test\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo test\n";
    let revision = store
        .save_file_as(
            pool,
            repo.id,
            owner.id,
            &codeyard::backend::repositories::store::FileWrite {
                branch: "main".into(),
                path: ".github/workflows/ci.yml".into(),
                message: "Add CI".into(),
                expected_revision: "unborn".into(),
                creating: true,
                deleting: false,
            },
            workflow,
            "Tester",
            "tester@example.invalid",
        )
        .await
        .unwrap();
    let blob = git2::Oid::hash_object(git2::ObjectType::Blob, workflow)
        .unwrap()
        .to_string();
    db::secrets::create_or_update_secret(
        pool,
        &encryptor,
        repo.id,
        "TOKEN",
        "private-value",
        Some(owner.id),
    )
    .await
    .unwrap();

    let ticket = cluster::create_invite(
        pool,
        core_key.public_key(),
        cluster::NodeAddress::of(&endpoint.addr()),
        &cluster::InviteOptions {
            roles: "actions-worker".parse().unwrap(),
            reusable: true,
            requires_approval: false,
            expires_in: Duration::from_secs(3600),
        },
    )
    .await
    .unwrap();
    let settings = cluster::Settings::from_lookup(&|name| match name {
        "CLUSTER_JOIN_TOKEN" => Some(ticket.encode()),
        "NODE_NAME" => Some("ci-worker".into()),
        _ => None,
    })
    .unwrap()
    .unwrap();
    let state_dir = root.path().join("worker-state");
    let (first_worker, _) = cluster::worker_context(&state_dir, settings).await.unwrap();
    first_worker.iroh.close().await;
    // Restart without the token, using only the persisted key and core address.
    let settings = cluster::Settings::from_lookup(&|name| match name {
        "IROH_BIND" => Some("127.0.0.1:0".into()),
        "NODE_NAME" => Some("ci-worker".into()),
        _ => None,
    })
    .unwrap()
    .unwrap();
    let (worker, peers) = cluster::worker_context(&state_dir, settings).await.unwrap();
    let other_key = cluster::NodeKey::generate();
    db::cluster::upsert_declared_nodes(
        pool,
        &[db::cluster::DeclaredMember {
            public_key: other_key.public_hex(),
            name: "ci-other".into(),
            roles: vec!["actions-worker".into()],
        }],
    )
    .await
    .unwrap();
    let other = client(&other_key).await;
    assert!(worker.pool.is_none());
    let peer = peers[0].clone();
    let remote = RunQueue::Remote {
        context: worker.clone(),
        peers,
    };
    let foreign = RunQueue::Remote {
        context: other.clone(),
        peers: vec![peer.clone()],
    };
    let fallback = RunQueue::Remote {
        context: worker.clone(),
        peers: vec![
            cluster::CorePeer {
                public_key: cluster::NodeKey::generate().public_hex(),
                address: cluster::NodeAddress {
                    addrs: vec!["127.0.0.1:9".parse().unwrap()],
                    relays: vec![],
                    tcp: vec![],
                },
            },
            peer.clone(),
        ],
    };
    assert!(fallback
        .claim("slot-1", CiExecutionMode::Local)
        .await
        .unwrap()
        .is_none());
    let worker_connection = worker
        .iroh
        .endpoint()
        .await
        .unwrap()
        .connect(peer.endpoint_addr().unwrap(), cluster::ALPN_RPC)
        .await
        .unwrap();
    let other_connection = other
        .iroh
        .endpoint()
        .await
        .unwrap()
        .connect(peer.endpoint_addr().unwrap(), cluster::ALPN_RPC)
        .await
        .unwrap();
    for (operation, worker_read) in rpc_operations::operations(&revision.sha) {
        if worker_read {
            repository_refused(&worker_connection, uuid, operation, "running CI claim").await;
        }
    }
    let queued = enqueue(pool, repo.id, owner.id, &revision.sha, &blob).await;
    // A queued run is not a claim.
    repository_refused(
        &worker_connection,
        uuid,
        rpc::Operation::GitSession(GitSession::new(GitService::UploadPack, true)),
        "running CI claim",
    )
    .await;
    let claimed = remote
        .claim("slot-1", CiExecutionMode::Local)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.run.id, queued.id);
    assert_eq!(queued.mode, "unassigned");
    assert_eq!(claimed.run.mode, "local");
    assert_eq!(claimed.repository, uuid);
    assert_eq!(claimed.owner, "ci_owner");
    assert_eq!(claimed.storage_node_id, Some(core_node.id));
    assert_eq!(claimed.secrets, [("TOKEN".into(), "private-value".into())]);
    let worker_store = remote.store(&claimed).unwrap();
    assert_eq!(worker_store.blob_by_id(&blob).await.unwrap(), workflow);
    let checkout = root.path().join("worker-checkout");
    worker_store
        .checkout(&checkout, &revision.sha)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(checkout.join(".github/workflows/ci.yml")).unwrap(),
        workflow
    );
    for (operation, worker_read) in rpc_operations::operations(&revision.sha) {
        if worker_read {
            repository_refused(&other_connection, uuid, operation, "running CI claim").await;
        } else {
            // A copy's pack is for storage nodes; every other call is for web and ssh nodes.
            let message = match operation {
                rpc::Operation::ReplicaPack { .. } => "requires a storage node",
                _ => "requires a web or ssh node",
            };
            repository_refused(&worker_connection, uuid, operation, message).await;
        }
    }
    let foreign_store = foreign.store(&claimed).unwrap();
    assert_eq!(
        foreign_store
            .checkout(&root.path().join("foreign-checkout"), &revision.sha)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    let other_uuid = Uuid::new_v4();
    let other_path = root.path().join(format!("{other_uuid}.git"));
    let other_store = RepoStore::from_disk_path(other_path.to_str().unwrap());
    other_store.initialize(Some(&store), "main").await.unwrap();
    db::repositories::records::create_repository(
        pool,
        owner.id,
        "other-project",
        None,
        true,
        other_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let claim_access = rpc::CiRunAccess {
        id: claimed.run.id,
        worker: claimed.run.worker.clone().unwrap(),
    };
    for access in [
        rpc::CiRunAccess {
            id: claim_access.id + 10_000,
            ..claim_access.clone()
        },
        rpc::CiRunAccess {
            worker: format!("{}/other-slot", worker.key.public_key()),
            ..claim_access.clone()
        },
    ] {
        repository_refused_with_claim(
            &worker_connection,
            uuid,
            rpc::Operation::BlobById { id: blob.clone() },
            Some(access),
            "running CI claim",
        )
        .await;
    }
    repository_refused_with_claim(
        &worker_connection,
        other_uuid,
        rpc::Operation::BlobById { id: blob.clone() },
        Some(claim_access),
        "running CI claim",
    )
    .await;
    for (operation, worker_read) in rpc_operations::operations(&revision.sha) {
        if worker_read {
            repository_refused(
                &worker_connection,
                other_uuid,
                operation,
                "running CI claim",
            )
            .await;
        }
    }
    assert_eq!(
        RepoStore::remote(other_uuid, core_node.id, &worker)
            .checkout(&root.path().join("other-checkout"), &revision.sha)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    // Storage-only callers are refused even the three worker reads.
    let nonworker_key = cluster::NodeKey::generate();
    db::cluster::upsert_declared_nodes(
        pool,
        &[db::cluster::DeclaredMember {
            public_key: nonworker_key.public_hex(),
            name: "not-a-worker".into(),
            roles: vec!["storage".into()],
        }],
    )
    .await
    .unwrap();
    let nonworker_context = client(&nonworker_key).await;
    let storage_connection = nonworker_context
        .iroh
        .endpoint()
        .await
        .unwrap()
        .connect(peer.endpoint_addr().unwrap(), cluster::ALPN_RPC)
        .await
        .unwrap();
    for (operation, _) in rpc_operations::operations(&revision.sha) {
        // A storage node may ask for a copy's pack, and is refused when it holds no copy.
        let message = match operation {
            rpc::Operation::ReplicaPack { .. } => "holds no copy of this repository",
            _ => "requires a web or ssh node",
        };
        repository_refused(&storage_connection, uuid, operation, message).await;
    }
    storage_connection.close(0u32.into(), b"test complete");
    assert_eq!(store.commit("main").await.unwrap().sha, revision.sha);
    assert!(!store
        .refs()
        .await
        .unwrap()
        .contains_key("refs/heads/worker-branch"));
    assert!(store.blob("main", "worker.txt").await.is_err());
    assert_eq!(
        remote.heartbeat("slot-1", queued.id).await.unwrap(),
        Some(false)
    );
    assert_eq!(foreign.heartbeat("slot-1", queued.id).await.unwrap(), None);
    assert_eq!(remote.heartbeat("slot-2", queued.id).await.unwrap(), None);
    assert!(foreign
        .logs("slot-1", queued.id, &[line("foreign")])
        .await
        .is_err());
    assert!(!foreign
        .finish("slot-1", queued.id, "success", None)
        .await
        .unwrap());
    assert!(
        subscriber.try_recv().is_err(),
        "foreign worker created no event"
    );
    remote
        .logs(
            "slot-1",
            queued.id,
            &[line("first"), line("binary\0output"), line("second")],
        )
        .await
        .unwrap();
    // Send malformed batches directly, bypassing the client's own validation.
    let connection = worker
        .iroh
        .endpoint()
        .await
        .unwrap()
        .connect(peer.endpoint_addr().unwrap(), cluster::ALPN_EVENTS)
        .await
        .unwrap();
    for lines in [
        vec![line("x"); 65],
        vec![line(&"x".repeat(16 * 1024 + 1))],
        vec![line(&"x".repeat(16 * 1024)); 17],
    ] {
        let reply = stream_request(
            &connection,
            json!({"worker":"slot-1", "operation":{"Logs":{"id":queued.id,"lines":lines}}}),
        )
        .await;
        assert!(reply["result"].get("Err").is_some(), "{reply}");
    }
    db::ci::request_ci_cancellation(pool, queued.run_id, owner.id)
        .await
        .unwrap();
    let cancelled_read = worker_store.blob_by_id(&blob).await.unwrap_err();
    assert_eq!(cancelled_read.kind, ErrorKind::Invalid);
    assert!(cancelled_read.message.contains("running CI claim"));
    assert_eq!(
        remote.heartbeat("slot-1", queued.id).await.unwrap(),
        Some(true)
    );
    remote
        .logs("slot-1", queued.id, &[line("stopping")])
        .await
        .unwrap();
    assert!(remote
        .finish("slot-1", queued.id, "cancelled", Some("Cancelled"))
        .await
        .unwrap());
    assert_eq!(
        db::ci::get_ci_run_by_run_id(pool, queued.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
    let event = tokio::time::timeout(Duration::from_secs(5), subscriber.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(&event, NotificationEvent::Created { user_id, .. } if *user_id == owner.id));
    let projected = current_event(pool, &session, owner.id, &event)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(projected, NotificationEvent::Created { kind, title, .. }
        if kind == "ci_completed" && title == "CI run cancelled")
    );
    assert!(current_event(pool, &session, other_user.id, &event)
        .await
        .unwrap()
        .is_none());
    assert!(!remote
        .finish("slot-1", queued.id, "cancelled", None)
        .await
        .unwrap());
    assert!(
        subscriber.try_recv().is_err(),
        "retry sends no second event"
    );
    let logs = db::ci::list_ci_logs(pool, queued.id, 100).await.unwrap();
    assert_eq!(
        logs.iter()
            .map(|line| line.line.as_str())
            .collect::<Vec<_>>(),
        ["first", "binary?output", "second", "stopping"]
    );
    assert!(remote
        .logs("slot-1", queued.id, &[line("after finish")])
        .await
        .is_err());

    for (operation, worker_read) in rpc_operations::operations(&revision.sha) {
        if worker_read {
            repository_refused(&worker_connection, uuid, operation, "running CI claim").await;
        }
    }
    assert_eq!(
        worker_store
            .checkout(&root.path().join("finished-checkout"), &revision.sha)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    // A later run on the same node and repository does not revive a stale store
    // from a different run or worker slot.
    let same_node_run = enqueue(pool, repo.id, owner.id, &revision.sha, &blob).await;
    let same_node_claim = remote
        .claim("slot-2", CiExecutionMode::Podman)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(same_node_claim.run.id, same_node_run.id);
    assert_eq!(
        worker_store
            .checkout(&root.path().join("same-node-stale-checkout"), &revision.sha)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    let same_node_store = remote.store(&same_node_claim).unwrap();
    assert_eq!(same_node_store.blob_by_id(&blob).await.unwrap(), workflow);
    assert!(remote
        .finish("slot-2", same_node_run.id, "success", None)
        .await
        .unwrap());
    assert_eq!(
        same_node_store.blob_by_id(&blob).await.unwrap_err().kind,
        ErrorKind::Invalid
    );

    let other_run = enqueue(pool, repo.id, owner.id, &revision.sha, &blob).await;
    let other_claim = foreign
        .claim("slot-1", CiExecutionMode::Local)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(other_claim.run.id, other_run.id);
    repository_refused(
        &worker_connection,
        uuid,
        rpc::Operation::GitSession(GitSession::new(GitService::UploadPack, true)),
        "running CI claim",
    )
    .await;
    assert_eq!(
        worker_store
            .checkout(&root.path().join("other-worker-checkout"), &revision.sha)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    foreign
        .store(&other_claim)
        .unwrap()
        .checkout(&root.path().join("own-checkout"), &revision.sha)
        .await
        .unwrap();
    assert!(foreign
        .finish("slot-1", other_run.id, "success", None)
        .await
        .unwrap());

    let fork = enqueue(pool, repo.id, owner.id, &revision.sha, &blob).await;
    if let db::DbPool::Postgres(pg) = pool {
        sqlx::query("UPDATE ci_runs SET is_fork = TRUE WHERE id = $1")
            .bind(fork.id)
            .execute(pg)
            .await
            .unwrap();
    }
    let claimed = remote
        .claim("slot-1", CiExecutionMode::Local)
        .await
        .unwrap()
        .unwrap();
    assert!(claimed.run.is_fork && claimed.secrets.is_empty());
    assert!(remote
        .finish("slot-1", fork.id, "success", None)
        .await
        .unwrap());

    let event = tokio::time::timeout(Duration::from_secs(5), subscriber.recv())
        .await
        .unwrap()
        .unwrap();
    let projected = current_event(pool, &session, owner.id, &event)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(projected, NotificationEvent::Created { kind, title, .. }
        if kind == "ci_completed" && title == "CI run success")
    );
    assert!(current_event(pool, &session, other_user.id, &event)
        .await
        .unwrap()
        .is_none());
    // Runs that finished after it announce their own notifications: none repeats this one.
    let NotificationEvent::Created {
        notification_id: first,
        ..
    } = &event
    else {
        panic!("a created event");
    };
    while let Ok(later) = subscriber.try_recv() {
        assert_eq!(later.user_id(), owner.id);
        assert!(
            !matches!(&later, NotificationEvent::Created { notification_id, .. } if notification_id == first),
            "a finish is announced once"
        );
    }

    // Exercise the production worker and Remote checkout, with the existing fake-act seam.
    let runner = root.path().join("runner");
    std::fs::write(&runner, "#!/bin/sh\nset -eu\ntest \"$GITSERV_REPO_OWNER\" = ci_owner\ntest \"$GITSERV_REPO_NAME\" = project\ntest \"$(stat -c %a .)\" = 700\nsecret=\nrepo=\nwhile test $# -gt 0; do\n case \"$1\" in\n --secret-file) shift; secret=$1;;\n --directory) shift; repo=$1;;\n esac\n shift\ndone\ngrep -q private-value \"$secret\"\ntest -f \"$repo/.github/workflows/ci.yml\"\necho CI_REMOTE_WORKER_PASSED\n").unwrap();
    std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    let actual = enqueue(pool, repo.id, owner.id, &revision.sha, &blob).await;
    assert_eq!(actual.mode, "unassigned");
    let workers = CiWorkers::new(
        remote.clone(),
        Arc::new(CiConfig {
            mode: CiExecutionMode::Local,
            act_binary: runner.to_str().unwrap().into(),
            max_active_runs: 1,
            run_timeout_seconds: 10,
            ..CiConfig::default()
        }),
        true,
        root.path(),
    );
    workers.startup().await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let run = db::ci::get_ci_run_by_run_id(pool, actual.run_id)
                .await
                .unwrap()
                .unwrap();
            if run.completed_at.is_some() {
                assert_eq!(run.status, "success", "{:?}", run.error_message);
                assert_eq!(run.mode, "local");
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    workers.shutdown().await.unwrap();
    assert!(db::ci::list_ci_logs(pool, actual.id, 100)
        .await
        .unwrap()
        .iter()
        .any(|line| line.line == "CI_REMOTE_WORKER_PASSED"));

    // A worker's cached storage address and still-running claim do not keep a
    // revoked destination node serving private repository data.
    let storage_run = enqueue(pool, repo.id, owner.id, &revision.sha, &blob).await;
    let storage_claim = remote
        .claim("storage-revoke", CiExecutionMode::Local)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(storage_claim.run.id, storage_run.id);
    let cached_storage = remote.store(&storage_claim).unwrap();
    assert_eq!(cached_storage.blob_by_id(&blob).await.unwrap(), workflow);
    cluster::revoke(pool, db::cluster::Authority::Operator, "ci-core")
        .await
        .unwrap();
    let revoked = cached_storage.blob_by_id(&blob).await.unwrap_err();
    assert!(revoked.is_storage_node_unavailable(), "{revoked:?}");
    cluster::approve(pool, db::cluster::Authority::Operator, "ci-core")
        .await
        .unwrap();
    assert_eq!(cached_storage.blob_by_id(&blob).await.unwrap(), workflow);
    assert!(remote
        .finish("storage-revoke", storage_run.id, "success", None)
        .await
        .unwrap());

    cluster::revoke(pool, db::cluster::Authority::Operator, "ci-worker")
        .await
        .unwrap();
    let reply = stream_request(
        &connection,
        json!({"worker":"slot-1","operation":{"Claim":{"mode":"local"}}}),
    )
    .await;
    assert!(
        reply["result"].get("Err").is_some(),
        "revoked peer on reused connection: {reply}"
    );
    assert!(remote
        .claim("slot-1", CiExecutionMode::Local)
        .await
        .is_err());
    connection.close(0u32.into(), b"test complete");
    let unknown = client(&cluster::NodeKey::generate()).await;
    let unknown_queue = RunQueue::Remote {
        context: unknown.clone(),
        peers: vec![peer.clone()],
    };
    assert!(unknown_queue
        .claim("slot-1", CiExecutionMode::Local)
        .await
        .is_err());
    let nonworker = RunQueue::Remote {
        context: nonworker_context.clone(),
        peers: vec![peer],
    };
    assert!(nonworker
        .claim("slot-1", CiExecutionMode::Local)
        .await
        .is_err());
    worker_connection.close(0u32.into(), b"test complete");
    other_connection.close(0u32.into(), b"test complete");
    nonworker_context.iroh.close().await;
    worker.iroh.close().await;
    other.iroh.close().await;
    unknown.iroh.close().await;
    router.shutdown().await.unwrap();
}

#[tokio::test]
async fn ci_repository_claim_checks_match_on_postgres_and_sqlite() {
    let postgres = native::PostgresDatabase::new().await;
    let sqlite = native::NativeDatabase::new().await;
    for pool in [&postgres.pool, &sqlite.pool] {
        let owner = db::users::accounts::create_user(pool, "claim_owner", None)
            .await
            .unwrap();
        let repo = db::repositories::records::create_repository(
            pool,
            owner.id,
            "project",
            None,
            true,
            "/fixture.git",
        )
        .await
        .unwrap();
        let other_repo = db::repositories::records::create_repository(
            pool,
            owner.id,
            "other-project",
            None,
            true,
            "/other.git",
        )
        .await
        .unwrap();
        let key = "a".repeat(64);

        for worker in [key.clone(), format!("{key}x/slot")] {
            let run = enqueue(pool, repo.id, owner.id, &"1".repeat(40), &"2".repeat(40)).await;
            db::ci::claim_ci_run(pool, &worker, "local", &[owner.id])
                .await
                .unwrap()
                .unwrap();
            assert!(
                !db::ci::ci_run_allows_repository_read(pool, run.id, repo.id, &worker, &key)
                    .await
                    .unwrap()
            );
            db::ci::finish_ci_run(pool, run.id, &worker, "success", None)
                .await
                .unwrap()
                .unwrap();
        }

        let worker = format!("{key}/slot");
        let run = enqueue(pool, repo.id, owner.id, &"3".repeat(40), &"4".repeat(40)).await;
        assert!(
            !db::ci::ci_run_allows_repository_read(pool, run.id, repo.id, &worker, &key)
                .await
                .unwrap()
        );
        db::ci::claim_ci_run(pool, &worker, "local", &[owner.id])
            .await
            .unwrap()
            .unwrap();
        assert!(
            db::ci::ci_run_allows_repository_read(pool, run.id, repo.id, &worker, &key)
                .await
                .unwrap()
        );
        let other_key = "b".repeat(64);
        for (run_id, repo_id, claimed_worker, public_key) in [
            (run.id + 10_000, repo.id, worker.as_str(), key.as_str()),
            (run.id, other_repo.id, worker.as_str(), key.as_str()),
            (run.id, repo.id, "other-worker", key.as_str()),
            (run.id, repo.id, worker.as_str(), other_key.as_str()),
        ] {
            assert!(!db::ci::ci_run_allows_repository_read(
                pool,
                run_id,
                repo_id,
                claimed_worker,
                public_key,
            )
            .await
            .unwrap());
        }

        db::ci::request_ci_cancellation(pool, run.run_id, owner.id)
            .await
            .unwrap();
        assert!(
            !db::ci::ci_run_allows_repository_read(pool, run.id, repo.id, &worker, &key)
                .await
                .unwrap()
        );
        assert_eq!(
            db::ci::heartbeat_ci_run(pool, run.id, &worker)
                .await
                .unwrap(),
            Some(true)
        );
        db::ci::append_ci_logs(pool, run.id, &worker, &[line("stopping")])
            .await
            .unwrap();
        db::ci::finish_ci_run(pool, run.id, &worker, "cancelled", None)
            .await
            .unwrap()
            .unwrap();

        for status in ["success", "failed"] {
            let run = enqueue(pool, repo.id, owner.id, &"5".repeat(40), &"6".repeat(40)).await;
            db::ci::claim_ci_run(pool, &worker, "local", &[owner.id])
                .await
                .unwrap()
                .unwrap();
            assert!(
                db::ci::ci_run_allows_repository_read(pool, run.id, repo.id, &worker, &key)
                    .await
                    .unwrap()
            );
            db::ci::finish_ci_run(pool, run.id, &worker, status, None)
                .await
                .unwrap()
                .unwrap();
            assert!(
                !db::ci::ci_run_allows_repository_read(pool, run.id, repo.id, &worker, &key)
                    .await
                    .unwrap()
            );
        }
    }
    sqlite.close().await;
}

#[tokio::test]
async fn sqlite_log_batches_check_ownership_and_bounds() {
    let database = native::NativeDatabase::new().await;
    let owner = db::users::accounts::create_user(&database.pool, "ci_owner", None)
        .await
        .unwrap();
    let repo = db::repositories::records::create_repository(
        &database.pool,
        owner.id,
        "project",
        None,
        true,
        "/fixture.git",
    )
    .await
    .unwrap();
    let run = enqueue(
        &database.pool,
        repo.id,
        owner.id,
        &"1".repeat(40),
        &"2".repeat(40),
    )
    .await;
    db::ci::claim_ci_run(&database.pool, "worker", "local", &[owner.id])
        .await
        .unwrap()
        .unwrap();
    assert!(
        db::ci::append_ci_logs(&database.pool, run.id, "other", &[line("not mine")])
            .await
            .is_err()
    );
    assert!(
        db::ci::append_ci_logs(&database.pool, run.id, "worker", &vec![line("x"); 65])
            .await
            .is_err()
    );
    db::ci::append_ci_logs(
        &database.pool,
        run.id,
        "worker",
        &[line("one"), line("two")],
    )
    .await
    .unwrap();
    assert_eq!(
        db::ci::list_ci_logs(&database.pool, run.id, 100)
            .await
            .unwrap()
            .len(),
        2
    );
    database.close().await;
}

#[tokio::test]
async fn worker_only_cli_has_no_database_or_listeners_and_stops_on_sigterm() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let root = tempfile::tempdir().unwrap();
    let web = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let ssh = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let key = cluster::NodeKey::generate();
    let state_dir = root.path().join("state");
    std::fs::create_dir(&state_dir).unwrap();
    std::fs::write(state_dir.join("node.key"), key.secret_hex()).unwrap();
    let peer = cluster::CorePeer {
        public_key: cluster::NodeKey::generate().public_hex(),
        address: cluster::NodeAddress {
            addrs: vec!["127.0.0.1:9".parse().unwrap()],
            relays: vec![],
            tcp: vec![],
        },
    };
    std::fs::write(
        state_dir.join("cluster-peer.json"),
        serde_json::to_vec(&peer).unwrap(),
    )
    .unwrap();
    // A record left by a dead worker is cleaned before the worker reports ready. A record
    // whose lock is held is a live claim and must not be touched.
    let records = state_dir.join("ci-containers");
    std::fs::create_dir(&records).unwrap();
    let run = Uuid::new_v4();
    let live = Uuid::new_v4();
    let socket = root.path().join("podman.sock");
    for id in [run, live] {
        std::fs::write(
            records.join(format!("{id}.json")),
            serde_json::to_vec(&json!({
                "socket": socket, "run_id": id,
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let live_claim = std::fs::File::open(records.join(format!("{live}.json"))).unwrap();
    use std::os::fd::AsRawFd;
    assert_eq!(
        unsafe { libc::flock(live_claim.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let podman = tokio::spawn(async move {
        // The first worker lists the run's containers and removes them. Nothing else asks.
        for (index, (status, body)) in [
            (200, r#"[{"Id":"workflow"},{"Id":"service"}]"#),
            (204, ""),
            (204, ""),
        ]
        .into_iter()
        .enumerate()
        {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut bytes = [0; 1024];
                let count = connection.read(&mut bytes).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&bytes[..count]);
            }
            let request = String::from_utf8(request).unwrap();
            let target = request
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
            if index == 0 {
                let filters = url
                    .query_pairs()
                    .find(|(name, _)| name == "filters")
                    .unwrap()
                    .1
                    .into_owned();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&filters).unwrap(),
                    json!({"label": [format!("com.codeyard.ci.run={run}")]})
                );
            } else {
                assert!(request.starts_with("DELETE "));
                assert_eq!(
                    url.path(),
                    if index == 1 {
                        "/v1.40/containers/workflow"
                    } else {
                        "/v1.40/containers/service"
                    }
                );
                assert!(url
                    .query_pairs()
                    .any(|(key, value)| key == "force" && value == "true"));
            }
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            connection.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let occupied = udp.local_addr().unwrap().to_string();
    for bind in [Some(occupied.as_str()), None, Some(""), Some(" \t ")] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_codeyard"));
        command.env_clear();
        if let Some(bind) = bind {
            command.env("IROH_BIND", bind);
        }
        let mut child = command
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("NODE_NAME", "cli-worker")
            .env("HTTP_ADDR", web.local_addr().unwrap().to_string())
            .env("SSH_ADDR", ssh.local_addr().unwrap().to_string())
            .current_dir(root.path())
            .args([
                "--state-dir",
                "state",
                "--repos-path",
                "repos",
                "serve",
                "--roles",
                "actions-worker",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stderr.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let line = output
                    .next_line()
                    .await
                    .unwrap()
                    .expect("worker exited before ready");
                if line.contains("database-free actions-worker ready") {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(!root.path().join("state/gitserv.db").exists());
        assert!(!root.path().join("repos").exists());
        assert_eq!(
            unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGTERM) },
            0
        );
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        let mut stopped_with = String::new();
        while let Some(line) = output.next_line().await.unwrap() {
            stopped_with.push_str(&line);
            stopped_with.push('\n');
        }
        assert!(
            status.success(),
            "worker with IROH_BIND {bind:?} exited with {status}:\n{stopped_with}"
        );
        assert_eq!(
            std::fs::read_to_string(state_dir.join("node.key")).unwrap(),
            key.secret_hex()
        );
    }
    tokio::time::timeout(Duration::from_secs(10), podman)
        .await
        .unwrap()
        .unwrap();
    assert!(!records.join(format!("{run}.json")).exists());
    assert!(records.join(format!("{live}.json")).exists());
}

#[test]
fn nonworker_roles_without_database_are_refused_before_creating_state() {
    let root = tempfile::tempdir().unwrap();
    for roles in [
        "web",
        "ssh",
        "storage",
        "web,storage",
        "web,storage,actions-worker",
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_codeyard"))
            .env_clear()
            .current_dir(root.path())
            .args(["--state-dir", "state", "serve", "--roles", roles])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("require DATABASE_URL"),
            "{:?}",
            output
        );
        assert!(!root.path().join("state").exists());
    }
}

async fn http_ci(
    pool: &db::DbPool,
    config: CiConfig,
) -> (
    tempfile::TempDir,
    codeyard::backend::app::AppState,
    db::models::User,
    db::models::Repository,
) {
    let db::users::transitions::SetupClaim::Completed(owner) =
        db::users::transitions::complete_setup(
            pool,
            "ci_owner",
            None,
            "fixture-hash",
            "public",
            None,
        )
        .await
        .unwrap()
    else {
        panic!("fresh CI installation");
    };
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (pool, None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner.id),
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let state = codeyard::backend::app::AppState::new(
        pool.clone(),
        root.path().to_owned(),
        Arc::new(
            codeyard::backend::storage::ObjectStore::open(root.path(), None)
                .await
                .unwrap(),
        ),
        None,
        config,
        None,
    );
    (root, state, *owner, repo)
}

async fn save_ci_file(client: &pulls::Client, path: &str, previous: &str, content: &str) -> String {
    use base64::Engine;
    let saved = client
        .expect(
            "POST",
            &format!("/api/repos/ci_owner/project/contents/{path}"),
            json!({"branch": "main", "expected_revision": previous, "message": "Add workflow",
            "content": base64::engine::general_purpose::STANDARD.encode(content)}),
            axum::http::StatusCode::CREATED,
        )
        .await;
    saved["sha"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn full_pipeline_tests_gate_deployment_and_report_the_workers_mode() {
    let postgres = native::PostgresDatabase::new().await;
    let sqlite = native::NativeDatabase::new().await;
    for pool in [&postgres.pool, &sqlite.pool] {
        let (root, state, owner, repo) = http_ci(
            pool,
            CiConfig {
                workflow_ci: ".github/workflows/tests.yml".into(),
                workflow_cd: ".github/workflows/deploy.yml".into(),
                ..CiConfig::default()
            },
        )
        .await;
        let ci = state.ci_runs.clone();
        let queue = RunQueue::Database {
            pool: pool.clone(),
            config: Arc::new(state.ci_runs.config().clone()),
            secrets: None,
            context: None,
            notifications: state.notification_broadcaster.clone(),
        };
        let router = codeyard::backend::api::create_router(state);
        let client = pulls::Client::login(&router, pool, owner.id).await;
        let workflow = "on: workflow_dispatch\njobs:\n  run:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n";
        let sha = save_ci_file(&client, ".github/workflows/tests.yml", "unborn", workflow).await;
        save_ci_file(&client, ".github/workflows/deploy.yml", &sha, workflow).await;
        let started = root.path().join("started");
        let allow = root.path().join("allow-tests");
        let runner = root.path().join("runner");
        std::fs::write(&runner, format!(
            "#!/bin/sh\nset -eu\nwhile test \"$1\" != -W; do shift; done\nshift\nname=$(basename \"$1\")\necho \"$name\" >> '{}'\nif test \"$name\" = tests.yml; then test -f '{}'; fi\n",
            started.display(), allow.display(),
        )).unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
        for expected in ["failed", "success"] {
            if expected == "success" {
                std::fs::write(&allow, "").unwrap();
            }
            let run = client
                .expect(
                    "POST",
                    "/api/ci/trigger",
                    json!({
                        "repo_owner": "ci_owner", "repo_name": "project", "branch": "main",
                        "event": "workflow_dispatch", "pipeline": "full",
                    }),
                    axum::http::StatusCode::OK,
                )
                .await;
            assert_eq!(run["mode"], "unassigned");
            let path = format!("/api/ci/jobs/{}", run["job_id"].as_str().unwrap());
            let workers = CiWorkers::new(
                queue.clone(),
                Arc::new(CiConfig {
                    mode: CiExecutionMode::Local,
                    act_binary: runner.to_str().unwrap().into(),
                    max_active_runs: 1,
                    run_timeout_seconds: 10,
                    ..CiConfig::default()
                }),
                true,
                root.path(),
            );
            workers.startup().await.unwrap();
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let current = client
                        .expect("GET", &path, json!(null), axum::http::StatusCode::OK)
                        .await;
                    if current["status"] == "failed" || current["status"] == "success" {
                        assert_eq!(current["status"], expected);
                        assert_eq!(current["mode"], "local");
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .unwrap();
            workers.shutdown().await.unwrap();
            assert_eq!(
                std::fs::read_to_string(&started).unwrap(),
                if expected == "failed" {
                    "tests.yml\n"
                } else {
                    "tests.yml\ntests.yml\ndeploy.yml\n"
                }
            );
        }
        let deduplicated = ci
            .trigger_run(codeyard::backend::ci::CiRunRequest {
                repo_id: repo.id,
                repo_owner: owner.username.clone(),
                repo_name: repo.name.clone(),
                branch: "main".into(),
                head_sha: None,
                event: "workflow_dispatch".into(),
                pull_request_id: None,
                inputs: None,
                pipeline: codeyard::backend::ci::CiPipelineKind::Full,
                workflow_paths: vec![
                    ".github/workflows/tests.yml".into(),
                    ".github/workflows/deploy.yml".into(),
                    ".github/workflows/tests.yml".into(),
                ],
                environment: None,
                triggered_by_id: owner.id,
            })
            .await
            .unwrap();
        assert_eq!(
            deduplicated.workflow_path,
            ".github/workflows/tests.yml,.github/workflows/deploy.yml"
        );
    }
    sqlite.close().await;
}

#[tokio::test]
async fn automatic_deployments_use_yaml_environments_and_explicit_targets_still_match() {
    let postgres = native::PostgresDatabase::new().await;
    let sqlite = native::NativeDatabase::new().await;
    for pool in [&postgres.pool, &sqlite.pool] {
        let (_root, state, owner, repo) = http_ci(pool, CiConfig::default()).await;
        let ci = state.ci_runs.clone();
        let router = codeyard::backend::api::create_router(state);
        let client = pulls::Client::login(&router, pool, owner.id).await;
        let sha = save_ci_file(&client, ".github/workflows/deploy.yml", "unborn",
            "on: [push, pull_request, workflow_dispatch]\njobs:\n  deploy:\n    runs-on: ubuntu-latest\n    environment: staging\n    steps:\n      - run: true\n").await;
        let runs = db::ci::list_ci_runs_for_repo(pool, repo.id, 10)
            .await
            .unwrap();
        assert_eq!(
            runs.len(),
            1,
            "the file API push must queue staging deployment"
        );
        assert_eq!(runs[0].event, "push");
        assert_eq!(runs[0].environment.as_deref(), Some("staging"));
        let deployment = db::ci::get_ci_deployment_by_run_id(pool, runs[0].id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deployment.environment, "staging");
        for (environment, status) in [
            (json!("production"), axum::http::StatusCode::BAD_REQUEST),
            (json!("staging"), axum::http::StatusCode::OK),
            (json!(null), axum::http::StatusCode::OK),
        ] {
            client.expect("POST", "/api/ci/trigger", json!({
                "repo_owner": "ci_owner", "repo_name": "project", "branch": "main",
                "event": "workflow_dispatch", "workflow_path": ".github/workflows/deploy.yml",
                "environment": environment,
            }), status).await;
        }
        let pull = db::pull_requests::records::create_pull_request(
            pool,
            db::pull_requests::records::NewPullRequest {
                repo_id: repo.id,
                head_repo_id: Some(repo.id),
                author_id: owner.id,
                title: "Staging",
                body: "",
                head_branch: "topic",
                head_sha: &sha,
                base_branch: "main",
                base_sha: &sha,
                is_draft: false,
            },
        )
        .await
        .unwrap();
        codeyard::backend::repositories::pull_requests::workflows::queue_pull_request_ci(
            pool, &ci, &repo, &pull, owner.id,
        )
        .await
        .unwrap();
        let runs = db::ci::list_ci_runs_for_repo(pool, repo.id, 10)
            .await
            .unwrap();
        let pr_run = runs.iter().find(|run| run.event == "pull_request").unwrap();
        assert_eq!(pr_run.environment.as_deref(), Some("staging"));
        save_ci_file(&client, ".github/workflows/deploy-default.yml", &sha,
            "on: push\njobs:\n  deploy:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n").await;
        let runs = db::ci::list_ci_runs_for_repo(pool, repo.id, 10)
            .await
            .unwrap();
        let fallback = runs
            .iter()
            .find(|run| run.workflow_path.ends_with("deploy-default.yml"))
            .unwrap();
        assert_eq!(fallback.environment.as_deref(), Some("production"));
    }
    sqlite.close().await;
}

/// A workflow that runs for pull requests into `main` only.
const WORKFLOW_FOR_MAIN: &str = "on:\n  pull_request:\n    branches: [main]\njobs:\n  check:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n";

/// A workflow that runs for pull requests into `main` and `release`.
const WORKFLOW_FOR_MAIN_AND_RELEASE: &str = "on:\n  pull_request:\n    branches: [main, release]\njobs:\n  check:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n";

/// Writes `path` on `branch` of the CI fixture repository, on top of `previous`.
async fn write_fixture_file(
    client: &pulls::Client,
    branch: &str,
    path: &str,
    previous: &str,
    content: &str,
) -> String {
    use base64::Engine;
    let saved = client
        .expect(
            "POST",
            &format!("/api/repos/ci_owner/project/contents/{path}"),
            json!({"branch": branch, "expected_revision": previous, "message": "Add file",
            "content": base64::engine::general_purpose::STANDARD.encode(content)}),
            axum::http::StatusCode::CREATED,
        )
        .await;
    saved["sha"].as_str().unwrap().to_owned()
}

async fn create_fixture_branch(client: &pulls::Client, name: &str, sha: &str) {
    client
        .expect(
            "POST",
            "/api/repos/ci_owner/project/branches",
            json!({"name": name, "sha": sha}),
            axum::http::StatusCode::OK,
        )
        .await;
}

/// Opens a pull request in the CI fixture repository and returns its number and id.
async fn open_fixture_pull(
    client: &pulls::Client,
    title: &str,
    head: &str,
    base: &str,
) -> (i64, i64) {
    let pull = client
        .expect(
            "POST",
            "/api/repos/ci_owner/project/pulls",
            json!({"title": title, "head_branch": head, "base_branch": base}),
            axum::http::StatusCode::CREATED,
        )
        .await;
    (
        pull["number"].as_i64().unwrap(),
        pull["id"].as_i64().unwrap(),
    )
}

/// The pull request of the CI fixture repository as it reads now.
async fn fixture_pull(client: &pulls::Client, number: i64) -> serde_json::Value {
    client
        .expect(
            "GET",
            &format!("/api/repos/ci_owner/project/pulls/{number}"),
            serde_json::Value::Null,
            axum::http::StatusCode::OK,
        )
        .await
}

/// Merges pull request `number` of the CI fixture repository with the commits it records now.
async fn merge_fixture_pull(client: &pulls::Client, number: i64) {
    let pull = fixture_pull(client, number).await;
    client
        .expect(
            "POST",
            &format!("/api/repos/ci_owner/project/pulls/{number}/merge"),
            json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"]}),
            axum::http::StatusCode::OK,
        )
        .await;
}

/// The queued pull request runs for pull request `pull_id`, on `branch`, for the commit `head`.
async fn queued_pull_request_runs(
    pool: &db::DbPool,
    repo_id: i64,
    pull_id: i64,
    branch: &str,
    head: &str,
) -> usize {
    db::ci::list_ci_runs_for_repo(pool, repo_id, 100)
        .await
        .unwrap()
        .iter()
        .filter(|run| {
            run.event == "pull_request"
                && run.pull_request_id == Some(pull_id)
                && run.branch == branch
                && run.head_sha.as_deref() == Some(head)
                && run.status == "queued"
        })
        .count()
}

/// A stacked pull request moves onto the base its bottom layer merged into. The workflows that run
/// on that base are then queued for its head, which did not change.
async fn retarget_queues_pull_request_workflows(pool: &db::DbPool) {
    let (_root, state, owner, repo) = http_ci(pool, CiConfig::default()).await;
    let router = codeyard::backend::api::create_router(state);
    let client = pulls::Client::login(&router, pool, owner.id).await;
    let main0 = write_fixture_file(
        &client,
        "main",
        ".github/workflows/check.yml",
        "unborn",
        WORKFLOW_FOR_MAIN,
    )
    .await;
    create_fixture_branch(&client, "feature-1", &main0).await;
    let one = write_fixture_file(&client, "feature-1", "one.txt", &main0, "one\n").await;
    let (bottom, _) = open_fixture_pull(&client, "Bottom", "feature-1", "main").await;
    create_fixture_branch(&client, "feature-2", &one).await;
    let two = write_fixture_file(&client, "feature-2", "two.txt", &one, "two\n").await;
    let (upper, upper_id) = open_fixture_pull(&client, "Upper", "feature-2", "feature-1").await;
    client
        .expect(
            "POST",
            &format!("/api/repos/ci_owner/project/pulls/{upper}/stack"),
            serde_json::Value::Null,
            axum::http::StatusCode::OK,
        )
        .await;
    // A layer runs the workflows of the trunk its stack merges into, so joining queues it for main
    // even before the bottom layer merges.
    assert_eq!(
        queued_pull_request_runs(pool, repo.id, upper_id, "main", &two).await,
        1,
        "joining the layer queues the workflows of the trunk it merges into"
    );

    merge_fixture_pull(&client, bottom).await;
    assert_eq!(fixture_pull(&client, upper).await["base_branch"], "main");
    assert_eq!(
        queued_pull_request_runs(pool, repo.id, upper_id, "main", &two).await,
        1,
        "the unchanged head is queued once for its new base"
    );
}

/// An explicit base change queues the workflows that run on the new base for the unchanged head.
/// A base that no workflow lists queues nothing.
async fn base_change_queues_pull_request_workflows(pool: &db::DbPool) {
    let (_root, state, owner, repo) = http_ci(pool, CiConfig::default()).await;
    let router = codeyard::backend::api::create_router(state);
    let client = pulls::Client::login(&router, pool, owner.id).await;
    let main0 = write_fixture_file(
        &client,
        "main",
        ".github/workflows/check.yml",
        "unborn",
        WORKFLOW_FOR_MAIN_AND_RELEASE,
    )
    .await;
    for branch in ["release", "develop", "topic"] {
        create_fixture_branch(&client, branch, &main0).await;
    }
    let head = write_fixture_file(&client, "topic", "topic.txt", &main0, "topic\n").await;
    let (number, id) = open_fixture_pull(&client, "Topic", "topic", "main").await;
    assert_eq!(
        queued_pull_request_runs(pool, repo.id, id, "main", &head).await,
        1,
        "opening it queues the workflows that run on main"
    );

    let pull = fixture_pull(&client, number).await;
    client
        .expect(
            "PUT",
            &format!("/api/repos/ci_owner/project/pulls/{number}"),
            json!({"expected_state": "open", "expected_updated": pull["updated_at"], "base": "develop"}),
            axum::http::StatusCode::OK,
        )
        .await;
    assert_eq!(
        queued_pull_request_runs(pool, repo.id, id, "develop", &head).await,
        0,
        "develop is not a branch the workflow lists"
    );

    let pull = fixture_pull(&client, number).await;
    client
        .expect(
            "PUT",
            &format!("/api/repos/ci_owner/project/pulls/{number}"),
            json!({"expected_state": "open", "expected_updated": pull["updated_at"], "base": "release"}),
            axum::http::StatusCode::OK,
        )
        .await;
    assert_eq!(
        queued_pull_request_runs(pool, repo.id, id, "release", &head).await,
        1,
        "the unchanged head is queued once for release"
    );
}

#[tokio::test]
async fn sqlite_retarget_queues_pull_request_workflows() {
    let database = native::NativeDatabase::new().await;
    retarget_queues_pull_request_workflows(&database.pool).await;
    database.close().await;
}

#[tokio::test]
async fn postgres_retarget_queues_pull_request_workflows() {
    let database = native::PostgresDatabase::new().await;
    retarget_queues_pull_request_workflows(&database.pool).await;
}

/// A layer of a stack can be dispatched by hand on the trunk its workflow declares. The dispatch
/// names the layer by its head commit, as the queueing does. The workflow applies to `main` only, so
/// the layer's own base is refused.
async fn manual_dispatch_upper_layer_contract(pool: &db::DbPool) {
    let (_root, state, owner, repo) = http_ci(pool, CiConfig::default()).await;
    let router = codeyard::backend::api::create_router(state);
    let client = pulls::Client::login(&router, pool, owner.id).await;
    let main0 = write_fixture_file(
        &client,
        "main",
        ".github/workflows/check.yml",
        "unborn",
        WORKFLOW_FOR_MAIN,
    )
    .await;
    create_fixture_branch(&client, "feature-1", &main0).await;
    let one = write_fixture_file(&client, "feature-1", "one.txt", &main0, "one\n").await;
    open_fixture_pull(&client, "Bottom", "feature-1", "main").await;
    create_fixture_branch(&client, "feature-2", &one).await;
    let two = write_fixture_file(&client, "feature-2", "two.txt", &one, "two\n").await;
    let (upper, upper_id) = open_fixture_pull(&client, "Upper", "feature-2", "feature-1").await;
    client
        .expect(
            "POST",
            &format!("/api/repos/ci_owner/project/pulls/{upper}/stack"),
            serde_json::Value::Null,
            axum::http::StatusCode::OK,
        )
        .await;

    let dispatch = |branch: &str| {
        json!({
            "repo_owner": "ci_owner", "repo_name": "project", "branch": branch,
            "event": "pull_request", "head_sha": two,
            "workflow_path": ".github/workflows/check.yml",
        })
    };
    client
        .expect(
            "POST",
            "/api/ci/trigger",
            dispatch("main"),
            axum::http::StatusCode::OK,
        )
        .await;
    let runs = db::ci::latest_ci_runs_for_commit(pool, repo.id, &two, upper_id)
        .await
        .unwrap();
    assert!(
        runs.iter().any(|run| run.pull_request_id == Some(upper_id)
            && run.branch == "main"
            && run.status == "queued"),
        "the dispatch on the trunk did not queue a run for the layer"
    );
    let (status, refused) = client
        .api("POST", "/api/ci/trigger", dispatch("feature-1"))
        .await;
    assert_ne!(status, axum::http::StatusCode::OK, "{refused}");
}

#[tokio::test]
async fn sqlite_manual_dispatch_runs_an_upper_layer_on_its_trunk() {
    let database = native::NativeDatabase::new().await;
    manual_dispatch_upper_layer_contract(&database.pool).await;
    database.close().await;
}

#[tokio::test]
async fn postgres_manual_dispatch_runs_an_upper_layer_on_its_trunk() {
    let database = native::PostgresDatabase::new().await;
    manual_dispatch_upper_layer_contract(&database.pool).await;
}

#[tokio::test]
async fn sqlite_base_change_queues_pull_request_workflows() {
    let database = native::NativeDatabase::new().await;
    base_change_queues_pull_request_workflows(&database.pool).await;
    database.close().await;
}

#[tokio::test]
async fn postgres_base_change_queues_pull_request_workflows() {
    let database = native::PostgresDatabase::new().await;
    base_change_queues_pull_request_workflows(&database.pool).await;
}

#[tokio::test]
async fn workflow_discovery_keeps_http_workers_responsive() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use codeyard::backend::{api::create_router, app::AppState, storage::ObjectStore};
    use std::{future::Future, time::Instant};
    use tower::ServiceExt;

    let sqlite = native::NativeDatabase::new().await;
    let postgres = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("workflows.git");
    {
        let git = git2::Repository::init_bare(&path).unwrap();
        let content = format!("on: push\nunused: [{}]\n", "0,".repeat(60_000));
        let blob = git.blob(content.as_bytes()).unwrap();
        let mut workflows = git.treebuilder(None).unwrap();
        for index in 0..64 {
            workflows
                .insert(format!("ci-{index:02}.yml"), blob, 0o100644)
                .unwrap();
        }
        let mut github = git.treebuilder(None).unwrap();
        github
            .insert("workflows", workflows.write().unwrap(), 0o040000)
            .unwrap();
        let mut tree = git.treebuilder(None).unwrap();
        tree.insert(".github", github.write().unwrap(), 0o040000)
            .unwrap();
        let tree = git.find_tree(tree.write().unwrap()).unwrap();
        let author = git2::Signature::now("Tester", "tester@example.invalid").unwrap();
        git.commit(
            Some("refs/heads/main"),
            &author,
            &author,
            "workflows",
            &tree,
            &[],
        )
        .unwrap();
    }
    for pool in [&sqlite.pool, &postgres.pool] {
        db::node_config::upsert_node_configuration(pool, "public", true)
            .await
            .unwrap();
        let owner = db::users::accounts::create_user(pool, "workflow_owner", None)
            .await
            .unwrap();
        let reader = db::users::accounts::create_user(pool, "workflow_reader", None)
            .await
            .unwrap();
        db::repositories::records::create_repository(
            pool,
            owner.id,
            "workflows",
            None,
            false,
            path.to_str().unwrap(),
        )
        .await
        .unwrap();
        let session = native::token();
        db::users::sessions::create_session(
            pool,
            &session,
            reader.id,
            None,
            None,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        let app = create_router(AppState::new(
            pool.clone(),
            root.path().to_owned(),
            Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
            None,
            CiConfig::default(),
            None,
        ));
        let request = app.oneshot(
            Request::builder()
                .uri("/api/ci/workflows?owner=workflow_owner&repo=workflows")
                .header("cookie", format!("session_id={session}"))
                .body(Body::empty())
                .unwrap(),
        );
        tokio::pin!(request);
        let mut longest_poll = Duration::ZERO;
        let response = std::future::poll_fn(|cx| {
            let start = Instant::now();
            let result = request.as_mut().poll(cx);
            longest_poll = longest_poll.max(start.elapsed());
            result
        })
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let workflows = body["workflows"].as_array().unwrap();
        assert_eq!(workflows.len(), 64);
        assert_eq!(workflows[0]["path"], ".github/workflows/ci-00.yml");
        assert_eq!(workflows[63]["path"], ".github/workflows/ci-63.yml");
        assert!(
            longest_poll < Duration::from_millis(250),
            "workflow discovery blocked the HTTP worker for {longest_poll:?}"
        );
    }
    sqlite.close().await;
}

#[test]
fn worker_shutdown_waits_for_descendant_and_workspace_cleanup() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let sqlite = native::NativeDatabase::new().await;
            let postgres = native::PostgresDatabase::new().await;
            for pool in [&sqlite.pool, &postgres.pool] {
                shutdown_with_blocked_workspace_cleanup(pool).await;
            }
            sqlite.close().await;
        });
}

async fn shutdown_with_blocked_workspace_cleanup(pool: &db::DbPool) {
    let owner = db::users::accounts::create_user(pool, "shutdown_owner", None)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let repository_id = Uuid::new_v4();
    let path = root.path().join(format!("{repository_id}.git"));
    let store = RepoStore::from_disk_path(path.to_str().unwrap());
    store.initialize(None, "main").await.unwrap();
    let repo = db::repositories::records::create_repository(
        pool,
        owner.id,
        "shutdown-project",
        None,
        true,
        path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let workflow =
        b"on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n";
    let revision = store
        .save_file_as(
            pool,
            repo.id,
            owner.id,
            &codeyard::backend::repositories::store::FileWrite {
                branch: "main".into(),
                path: ".github/workflows/ci.yml".into(),
                message: "Add CI".into(),
                expected_revision: "unborn".into(),
                creating: true,
                deleting: false,
            },
            workflow,
            "Tester",
            "tester@example.invalid",
        )
        .await
        .unwrap();
    let blob = git2::Oid::hash_object(git2::ObjectType::Blob, workflow)
        .unwrap()
        .to_string();
    let run = enqueue(pool, repo.id, owner.id, &revision.sha, &blob).await;

    let pids = root.path().join("runner-pids");
    let workspace_path = root.path().join("workspace-path");
    let marker = root.path().join("delayed-marker");
    let runner = root.path().join("runner");
    std::fs::write(
        &runner,
        format!(
            "#!/bin/sh\nset -eu\npwd > '{}'\nmkdir -p home/dependencies\ntouch home/dependencies/artifact\n(trap '' TERM; sleep 10; touch '{}') </dev/null >/dev/null 2>&1 & child=$!\nprintf '%s %s\\n' \"$$\" \"$child\" > '{}'\nwait\n",
            workspace_path.display(),
            marker.display(),
            pids.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = Arc::new(CiConfig {
        mode: CiExecutionMode::Local,
        act_binary: runner.to_string_lossy().into_owned(),
        trusted_actor_ids: BTreeSet::from([owner.id]),
        max_active_runs: 1,
        run_timeout_seconds: 30,
        ..CiConfig::default()
    });
    let workers = CiWorkers::new(
        RunQueue::Database {
            pool: pool.clone(),
            config: config.clone(),
            secrets: None,
            context: None,
            notifications: Arc::new(NotificationBroadcaster::new(pool)),
        },
        config,
        true,
        root.path(),
    );
    workers.startup().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !pids.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let workspace =
        std::path::PathBuf::from(std::fs::read_to_string(workspace_path).unwrap().trim());
    // Hold the only blocking thread. Cleanup must queue here, not delete on the
    // async worker, and shutdown must retain it after the run's result is recorded.
    let (release, blocked) = std::sync::mpsc::channel();
    let (started, ready) = tokio::sync::oneshot::channel();
    let blocker = tokio::task::spawn_blocking(move || {
        started.send(()).unwrap();
        let _ = blocked.recv();
    });
    ready.await.unwrap();
    let mut stopping = tokio::spawn({
        let workers = workers.clone();
        async move { workers.shutdown().await }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let run = db::ci::get_ci_run_by_run_id(pool, run.run_id)
                .await
                .unwrap()
                .unwrap();
            if run.status == "failed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(workspace.join("home/dependencies/artifact").exists());
    assert!(!stopping.is_finished());
    if matches!(pool, db::DbPool::Sqlite(_)) {
        let error = tokio::time::timeout(Duration::from_secs(35), &mut stopping)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.to_string(), "CI workspace cleanup is still running");
        assert!(workspace.exists(), "shutdown must retain queued cleanup");
        release.send(()).unwrap();
        workers.shutdown().await.unwrap();
    } else {
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), stopping)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    blocker.await.unwrap();
    assert!(!workspace.exists());

    let finished = db::ci::get_ci_run_by_run_id(pool, run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(finished.status, "failed");
    assert_eq!(
        finished.error_message.as_deref(),
        Some("The CI worker shut down during the run")
    );
    for pid in std::fs::read_to_string(pids)
        .unwrap()
        .split_whitespace()
        .map(|pid| pid.parse::<i32>().unwrap())
    {
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!marker.exists());
}

#[tokio::test]
async fn database_workers_recheck_membership_before_consuming_runs() {
    let postgres = native::PostgresDatabase::new().await;
    let sqlite = native::NativeDatabase::new().await;
    for pool in [&postgres.pool, &sqlite.pool] {
        let root = tempfile::tempdir().unwrap();
        let key = cluster::NodeKey::generate();
        let node = db::cluster::register_own_node(
            pool,
            &key.public_hex(),
            "database-worker",
            &["actions-worker".into(), "storage".into()],
            "test",
        )
        .await
        .unwrap();
        let mut context = client(&key).await;
        context.pool = Some(pool.clone());
        context.node_id = node.id;
        context.repos_path = root.path().to_owned();
        db::cluster::record_node_heartbeat(
            pool,
            &key.public_hex(),
            "test",
            &serde_json::to_string(&cluster::NodeAddress::of(
                &context.iroh.endpoint().await.unwrap().addr(),
            ))
            .unwrap(),
            Some(1),
        )
        .await
        .unwrap();
        let owner = db::users::accounts::create_user(pool, "database_worker_owner", None)
            .await
            .unwrap();
        let path = root.path().join(format!("{}.git", Uuid::new_v4()));
        let store = RepoStore::from_disk_path(path.to_str().unwrap());
        store.initialize(None, "main").await.unwrap();
        let repo = db::repositories::records::create_repository(
            pool,
            owner.id,
            "project",
            None,
            true,
            path.to_str().unwrap(),
        )
        .await
        .unwrap();
        let workflow = b"on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n";
        let revision = store
            .save_file_as(
                pool,
                repo.id,
                owner.id,
                &codeyard::backend::repositories::store::FileWrite {
                    branch: "main".into(),
                    path: ".github/workflows/ci.yml".into(),
                    message: "Add CI".into(),
                    expected_revision: "unborn".into(),
                    creating: true,
                    deleting: false,
                },
                workflow,
                "Tester",
                "tester@example.invalid",
            )
            .await
            .unwrap();
        let blob = git2::Oid::hash_object(git2::ObjectType::Blob, workflow)
            .unwrap()
            .to_string();
        let runner = root.path().join("runner");
        std::fs::write(&runner, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = Arc::new(CiConfig {
            mode: CiExecutionMode::Local,
            act_binary: runner.to_str().unwrap().into(),
            trusted_actor_ids: BTreeSet::from([owner.id]),
            max_active_runs: 1,
            run_timeout_seconds: 10,
            ..CiConfig::default()
        });
        let workers = CiWorkers::new(
            RunQueue::Database {
                pool: pool.clone(),
                config: config.clone(),
                secrets: None,
                context: Some(context.clone()),
                notifications: Arc::new(NotificationBroadcaster::new(pool)),
            },
            config,
            true,
            root.path(),
        );
        workers.startup().await.unwrap();
        // Keep the same worker alive across every membership change.
        for state in [
            "active",
            "revoked",
            "pending",
            "role-removed",
            "key-changed",
        ] {
            if matches!(state, "revoked" | "pending") {
                db::cluster::set_node_status(
                    pool,
                    db::cluster::Authority::Operator,
                    &node.name,
                    state,
                )
                .await
                .unwrap();
            } else if state == "role-removed" {
                db::cluster::upsert_declared_nodes(
                    pool,
                    &[db::cluster::DeclaredMember {
                        public_key: key.public_hex(),
                        name: node.name.clone(),
                        roles: vec!["storage".into()],
                    }],
                )
                .await
                .unwrap();
            } else if state == "key-changed" {
                let replacement = cluster::NodeKey::generate().public_hex();
                match pool {
                    db::DbPool::Postgres(pg) => {
                        sqlx::query("UPDATE nodes SET public_key = $1 WHERE id = $2")
                            .bind(&replacement)
                            .bind(node.id)
                            .execute(pg)
                            .await
                            .unwrap();
                    }
                    db::DbPool::Sqlite(lite) => {
                        sqlx::query("UPDATE nodes SET public_key = ?1 WHERE id = ?2")
                            .bind(&replacement)
                            .bind(node.id)
                            .execute(lite)
                            .await
                            .unwrap();
                    }
                }
            }
            let queued = enqueue(pool, repo.id, owner.id, &revision.sha, &blob).await;
            if state != "active" {
                tokio::time::timeout(Duration::from_secs(4), async {
                    loop {
                        let current = db::ci::get_ci_run_by_run_id(pool, queued.run_id)
                            .await
                            .unwrap()
                            .unwrap();
                        assert_eq!(current.status, "queued", "{state}");
                        assert!(current.worker.is_none(), "{state}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                })
                .await
                .unwrap_err();
                if state == "key-changed" {
                    match pool {
                        db::DbPool::Postgres(pg) => {
                            sqlx::query("UPDATE nodes SET public_key = $1 WHERE id = $2")
                                .bind(key.public_hex())
                                .bind(node.id)
                                .execute(pg)
                                .await
                                .unwrap();
                        }
                        db::DbPool::Sqlite(lite) => {
                            sqlx::query("UPDATE nodes SET public_key = ?1 WHERE id = ?2")
                                .bind(key.public_hex())
                                .bind(node.id)
                                .execute(lite)
                                .await
                                .unwrap();
                        }
                    }
                }
                db::cluster::upsert_declared_nodes(
                    pool,
                    &[db::cluster::DeclaredMember {
                        public_key: key.public_hex(),
                        name: node.name.clone(),
                        roles: node.roles.clone(),
                    }],
                )
                .await
                .unwrap();
                cluster::approve(pool, db::cluster::Authority::Operator, &node.name)
                    .await
                    .unwrap();
            }
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let current = db::ci::get_ci_run_by_run_id(pool, queued.run_id)
                        .await
                        .unwrap()
                        .unwrap();
                    if current.completed_at.is_some() {
                        assert_eq!(
                            current.status, "success",
                            "{state}: {:?}",
                            current.error_message
                        );
                        assert!(current
                            .worker
                            .unwrap()
                            .starts_with(&format!("{}/", key.public_hex())));
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .unwrap();
        }
        workers.shutdown().await.unwrap();
        context.iroh.close().await;
    }
    sqlite.close().await;
}

#[tokio::test]
async fn database_sweeper_stops_before_pool_close() {
    let database = native::NativeDatabase::new().await;
    let config = Arc::new(CiConfig::default());
    let workers = CiWorkers::new(
        RunQueue::Database {
            pool: database.pool.clone(),
            config: config.clone(),
            secrets: None,
            context: None,
            notifications: Arc::new(NotificationBroadcaster::new(&database.pool)),
        },
        config,
        false,
        std::path::Path::new("/unused"),
    );
    workers.startup().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), workers.shutdown())
        .await
        .unwrap()
        .unwrap();
    database.pool.close().await;
}

#[tokio::test]
async fn startup_does_not_wait_for_a_podman_socket_that_never_answers() {
    let database = native::NativeDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    // The socket accepts connections and never answers, like a Podman service that has hung.
    let socket = root.path().join("podman.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    // Each record costs one 15-second request timeout, so waiting for all four would take a
    // minute.
    let state = root.path().join("state");
    let records = state.join("ci-containers");
    std::fs::create_dir_all(&records).unwrap();
    for _ in 0..4 {
        let record = json!({ "socket": socket.to_str().unwrap(), "run_id": Uuid::new_v4() });
        std::fs::write(
            records.join(format!("{}.json", Uuid::new_v4())),
            record.to_string(),
        )
        .unwrap();
    }
    let config = Arc::new(CiConfig::default());
    let workers = CiWorkers::new(
        RunQueue::Database {
            pool: database.pool.clone(),
            config: config.clone(),
            secrets: None,
            context: None,
            notifications: Arc::new(NotificationBroadcaster::new(&database.pool)),
        },
        config,
        false,
        &state,
    );
    let started = std::time::Instant::now();
    workers.startup().await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(45),
        "startup waited {:?} for the Podman socket",
        started.elapsed()
    );
    database.close().await;
}

#[tokio::test]
async fn a_run_whose_commit_is_missing_fails_and_frees_its_slot() {
    let postgres = native::PostgresDatabase::new().await;
    let sqlite = native::NativeDatabase::new().await;
    for pool in [&postgres.pool, &sqlite.pool] {
        let root = tempfile::tempdir().unwrap();
        let owner = db::users::accounts::create_user(pool, "missing_commit_owner", None)
            .await
            .unwrap();
        let path = root.path().join(format!("{}.git", Uuid::new_v4()));
        let store = RepoStore::from_disk_path(path.to_str().unwrap());
        store.initialize(None, "main").await.unwrap();
        let repo = db::repositories::records::create_repository(
            pool,
            owner.id,
            "project",
            None,
            true,
            path.to_str().unwrap(),
        )
        .await
        .unwrap();
        // Neither commit exists. With one slot, the second run is claimed only if the failed
        // first run gives its slot back.
        let blob = git2::Oid::hash_object(git2::ObjectType::Blob, b"on: push\n")
            .unwrap()
            .to_string();
        let first = enqueue(pool, repo.id, owner.id, &"1".repeat(40), &blob).await;
        let second = enqueue(pool, repo.id, owner.id, &"2".repeat(40), &blob).await;
        let config = Arc::new(CiConfig {
            mode: CiExecutionMode::Local,
            act_binary: root.path().join("runner").to_string_lossy().into_owned(),
            trusted_actor_ids: BTreeSet::from([owner.id]),
            max_active_runs: 1,
            ..CiConfig::default()
        });
        let workers = CiWorkers::new(
            RunQueue::Database {
                pool: pool.clone(),
                config: config.clone(),
                secrets: None,
                context: None,
                notifications: Arc::new(NotificationBroadcaster::new(pool)),
            },
            config,
            true,
            root.path(),
        );
        workers.startup().await.unwrap();
        for run in [&first, &second] {
            let finished = tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let current = db::ci::get_ci_run_by_run_id(pool, run.run_id)
                        .await
                        .unwrap()
                        .unwrap();
                    if current.completed_at.is_some() {
                        break current;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(finished.status, "failed");
            assert!(
                finished
                    .error_message
                    .as_deref()
                    .is_some_and(|message| message.starts_with("CI could not run:")),
                "{:?}",
                finished.error_message
            );
        }
        tokio::time::timeout(Duration::from_secs(10), workers.shutdown())
            .await
            .unwrap()
            .unwrap();
    }
    sqlite.close().await;
}

/// Sets a run's stored log totals directly, so a test can start a run near its limit
/// without writing the limit's worth of lines.
async fn set_log_totals(pool: &db::DbPool, run: i64, lines: i64, bytes: i64) {
    match pool {
        db::DbPool::Postgres(native) => {
            sqlx::query("UPDATE ci_runs SET log_lines = $2, log_bytes = $3 WHERE id = $1")
                .bind(run)
                .bind(lines)
                .bind(bytes)
                .execute(native)
                .await
                .unwrap();
        }
        db::DbPool::Sqlite(native) => {
            sqlx::query("UPDATE ci_runs SET log_lines = ?2, log_bytes = ?3 WHERE id = ?1")
                .bind(run)
                .bind(lines)
                .bind(bytes)
                .execute(native)
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn binary_logs_are_printable_and_counted_before_limits_on_both_engines() {
    let postgres = native::PostgresDatabase::new().await;
    let sqlite = native::NativeDatabase::new().await;
    for pool in [&postgres.pool, &sqlite.pool] {
        let owner = db::users::accounts::create_user(pool, "binary_owner", None)
            .await
            .unwrap();
        let repo = db::repositories::records::create_repository(
            pool,
            owner.id,
            "project",
            None,
            false,
            "/fixture.git",
        )
        .await
        .unwrap();
        let run = enqueue(pool, repo.id, owner.id, &"1".repeat(40), &"2".repeat(40)).await;
        db::ci::claim_ci_run(pool, "worker", "local", &[owner.id])
            .await
            .unwrap()
            .unwrap();
        db::ci::append_ci_logs(
            pool,
            run.id,
            "worker",
            &[
                line("before"),
                LogLine {
                    stage: "ci\0stage".into(),
                    line: "binary\0\u{1}\u{7}\u{1b}\u{7f}\tλ".into(),
                    level: "info".into(),
                },
                line("after"),
            ],
        )
        .await
        .unwrap();
        let stored = db::ci::list_ci_logs(pool, run.id, 100).await.unwrap();
        assert_eq!(
            stored
                .iter()
                .map(|line| line.line.as_str())
                .collect::<Vec<_>>(),
            ["before", "binary?????\tλ", "after"]
        );
        assert_eq!(stored[1].stage, "ci?stage");
        let expected_bytes: i64 = stored
            .iter()
            .map(|line| (line.stage.len() + line.line.len()) as i64)
            .sum();
        let totals: (i64, i64) = match pool {
            db::DbPool::Postgres(raw) => {
                sqlx::query_as("SELECT log_lines,log_bytes FROM ci_runs WHERE id=$1")
                    .bind(run.id)
                    .fetch_one(raw)
                    .await
                    .unwrap()
            }
            db::DbPool::Sqlite(raw) => {
                sqlx::query_as("SELECT log_lines,log_bytes FROM ci_runs WHERE id=?1")
                    .bind(run.id)
                    .fetch_one(raw)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(totals, (3, expected_bytes));
        for batch in [
            vec![line(&"\0".repeat(16 * 1024 + 1))],
            vec![LogLine {
                stage: "\0".repeat(256),
                ..line("bounded stage")
            }],
            vec![line(&"\0".repeat(16 * 1024)); 17],
        ] {
            assert!(db::ci::append_ci_logs(pool, run.id, "worker", &batch)
                .await
                .is_err());
        }
        assert_eq!(
            db::ci::list_ci_logs(pool, run.id, 100).await.unwrap().len(),
            3
        );
        let queue = RunQueue::Database {
            pool: pool.clone(),
            config: Arc::new(CiConfig::default()),
            secrets: None,
            context: None,
            notifications: Arc::new(NotificationBroadcaster::new(pool)),
        };
        queue
            .logs(
                "worker",
                run.id,
                &[
                    line("neighbor before"),
                    line(&"\0".repeat(4096)),
                    line("neighbor after"),
                ],
            )
            .await
            .unwrap();
        let stored = db::ci::list_ci_logs(pool, run.id, 100).await.unwrap();
        assert_eq!(stored.len(), 6);
        assert_eq!(stored[3].line, "neighbor before");
        assert_eq!(stored[4].line, "?".repeat(4096));
        assert_eq!(stored[5].line, "neighbor after");
        assert!(queue
            .finish("worker", run.id, "success", None)
            .await
            .unwrap());
        assert_eq!(
            db::ci::get_ci_run_by_run_id(pool, run.run_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "success"
        );
    }
}

#[tokio::test]
async fn control_byte_batches_are_accepted_whole_on_both_engines() {
    let postgres = native::PostgresDatabase::new().await;
    let sqlite = native::NativeDatabase::new().await;
    for pool in [&postgres.pool, &sqlite.pool] {
        let owner = db::users::accounts::create_user(pool, "control_owner", None)
            .await
            .unwrap();
        let repo = db::repositories::records::create_repository(
            pool,
            owner.id,
            "project",
            None,
            false,
            "/fixture.git",
        )
        .await
        .unwrap();
        let run = enqueue(pool, repo.id, owner.id, &"1".repeat(40), &"2".repeat(40)).await;
        db::ci::claim_ci_run(pool, "worker", "local", &[owner.id])
            .await
            .unwrap()
            .unwrap();
        let queue = RunQueue::Database {
            pool: pool.clone(),
            config: Arc::new(CiConfig::default()),
            secrets: None,
            context: None,
            notifications: Arc::new(NotificationBroadcaster::new(pool)),
        };
        // Sixteen 4 KiB lines of control bytes and a neighbor reach the core as one batch.
        // Stored as `?`, the batch stays far below the 256 KiB limit, so it is accepted whole.
        let mut batch: Vec<LogLine> = (0..16).map(|_| line(&"\u{1}".repeat(4096))).collect();
        batch.push(line("neighbor"));
        queue.logs("worker", run.id, &batch).await.unwrap();
        let stored = db::ci::list_ci_logs(pool, run.id, 100).await.unwrap();
        assert_eq!(
            stored.len(),
            17,
            "the batch must not be truncated or dropped"
        );
        assert!(stored[..16]
            .iter()
            .all(|line| line.line == "?".repeat(4096)));
        assert_eq!(stored[16].line, "neighbor");
        assert!(queue
            .finish("worker", run.id, "success", None)
            .await
            .unwrap());
        assert_eq!(
            db::ci::get_ci_run_by_run_id(pool, run.run_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "success"
        );
    }
}

#[tokio::test]
async fn log_totals_close_a_run_at_its_limit_with_one_truncation_line() {
    let postgres = native::PostgresDatabase::new().await;
    let sqlite = native::NativeDatabase::new().await;
    for pool in [&postgres.pool, &sqlite.pool] {
        let owner = db::users::accounts::create_user(pool, "log_limit_owner", None)
            .await
            .unwrap();
        let repo = db::repositories::records::create_repository(
            pool,
            owner.id,
            "project",
            None,
            true,
            "/fixture.git",
        )
        .await
        .unwrap();
        let run = enqueue(pool, repo.id, owner.id, &"1".repeat(40), &"2".repeat(40)).await;
        db::ci::claim_ci_run(pool, "worker", "local", &[owner.id])
            .await
            .unwrap()
            .unwrap();
        set_log_totals(
            pool,
            run.id,
            codeyard::backend::ci::logs::MAX_RUN_LOG_LINES - 2,
            0,
        )
        .await;

        db::ci::append_ci_logs(
            pool,
            run.id,
            "worker",
            &[line("kept"), line("dropped"), line("dropped too")],
        )
        .await
        .unwrap();
        let stored = db::ci::list_ci_logs(pool, run.id, 100).await.unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].line, "kept");
        assert_eq!(stored[1].level, "warn");

        db::ci::append_ci_logs(pool, run.id, "worker", &[line("after the limit")])
            .await
            .unwrap();
        assert_eq!(
            db::ci::list_ci_logs(pool, run.id, 100).await.unwrap().len(),
            2
        );
        db::ci::finish_ci_run(pool, run.id, "worker", "success", None)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn claims_refuse_secret_sets_too_large_for_one_claim() {
    let postgres = native::PostgresDatabase::new().await;
    let sqlite = native::NativeDatabase::new().await;
    for pool in [&postgres.pool, &sqlite.pool] {
        let owner = db::users::accounts::create_user(pool, "claim_secret_owner", None)
            .await
            .unwrap();
        // A claim needs a repository path named by its UUID.
        let repo = db::repositories::records::create_repository(
            pool,
            owner.id,
            "secret-project",
            None,
            true,
            &format!("/{}.git", Uuid::new_v4()),
        )
        .await
        .unwrap();
        let encryptor = Arc::new(SecretEncryptor::new(&[7; 32]));
        let queue = RunQueue::Database {
            pool: pool.clone(),
            config: Arc::new(CiConfig {
                trusted_actor_ids: BTreeSet::from([owner.id]),
                ..CiConfig::default()
            }),
            secrets: Some(encryptor.clone()),
            context: None,
            notifications: Arc::new(NotificationBroadcaster::new(pool)),
        };
        // Twenty-one maximal values fit in one claim; twenty-two do not.
        let value = "v".repeat(db::secrets::MAX_SECRET_VALUE_BYTES);
        for index in 0..21 {
            db::secrets::create_or_update_secret(
                pool,
                &encryptor,
                repo.id,
                &format!("SECRET_{index:02}"),
                &value,
                Some(owner.id),
            )
            .await
            .unwrap();
        }
        let accepted = enqueue(pool, repo.id, owner.id, &"1".repeat(40), &"2".repeat(40)).await;
        let claim = queue
            .claim("worker-1", CiExecutionMode::Local)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claim.run.id, accepted.id);
        assert_eq!(claim.secrets.len(), 21);

        // The save guard refuses a 22nd value, so the set never grows past one claim.
        let refused_save = db::secrets::create_or_update_secret(
            pool,
            &encryptor,
            repo.id,
            "SECRET_21",
            &value,
            Some(owner.id),
        )
        .await;
        assert!(
            matches!(refused_save, Err(db::secrets::SecretsError::TotalTooLarge)),
            "{refused_save:?}"
        );
        // A row past that limit, as an older version or a manual fix could leave one, must still
        // be refused by the claim itself.
        let (encrypted, nonce) = encryptor.encrypt(value.as_bytes()).unwrap();
        match pool {
            db::DbPool::Postgres(pg) => {
                sqlx::query(
                    "INSERT INTO repo_secrets (repo_id, name, encrypted_value, nonce, created_by_id) \
                     VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(repo.id)
                .bind("SECRET_21")
                .bind(&encrypted)
                .bind(&nonce[..])
                .bind(owner.id)
                .execute(pg)
                .await
                .unwrap();
            }
            db::DbPool::Sqlite(lite) => {
                sqlx::query(
                    "INSERT INTO repo_secrets (repo_id, name, encrypted_value, nonce, created_by_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .bind(repo.id)
                .bind("SECRET_21")
                .bind(&encrypted)
                .bind(&nonce[..])
                .bind(owner.id)
                .execute(lite)
                .await
                .unwrap();
            }
        }
        let refused = enqueue(pool, repo.id, owner.id, &"3".repeat(40), &"4".repeat(40)).await;
        assert!(queue
            .claim("worker-2", CiExecutionMode::Local)
            .await
            .is_err());
        let finished = db::ci::get_ci_run_by_run_id(pool, refused.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(finished.status, "failed");
        assert_eq!(
            finished.error_message.as_deref(),
            Some("CI could not load the repository or secrets")
        );
    }
}

struct CiAuthorization(mpsc::UnboundedSender<()>);

impl tracing_subscriber::Layer<tracing_subscriber::Registry> for CiAuthorization {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, tracing_subscriber::Registry>,
    ) {
        if event.metadata().name() == "ci_request_authorized" {
            let _ = self.0.send(());
        }
    }
}

/// Counts the connections that reach the CI queue. A worker opens one for each request.
#[derive(Debug)]
struct CountingCi {
    inner: cluster::ci::CiHandler,
    requests: Arc<AtomicUsize>,
    dispatch: tracing::Dispatch,
}

impl ProtocolHandler for CountingCi {
    async fn accept(
        &self,
        connection: iroh::endpoint::Connection,
    ) -> Result<(), iroh::protocol::AcceptError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.inner
            .accept(connection)
            .with_subscriber(self.dispatch.clone())
            .await
    }
}

/// A core node that serves the CI queue and the repository RPC, with a repository whose
/// workflow can run, and a database-free worker that is an active actions-worker.
struct CiFixture {
    database: native::PostgresDatabase,
    root: tempfile::TempDir,
    endpoint: iroh::Endpoint,
    router: iroh::protocol::Router,
    requests: Arc<AtomicUsize>,
    authorized: mpsc::UnboundedReceiver<()>,
    worker: NodeContext,
    peer: cluster::CorePeer,
    owner: i64,
    repo: i64,
    sha: String,
    blob: String,
}

impl CiFixture {
    fn pool(&self) -> &db::DbPool {
        &self.database.pool
    }

    fn remote(&self) -> RunQueue {
        RunQueue::Remote {
            context: self.worker.clone(),
            peers: vec![self.peer.clone()],
        }
    }

    async fn queue_run(&self) -> db::models::CiRun {
        enqueue(self.pool(), self.repo, self.owner, &self.sha, &self.blob).await
    }

    async fn authorized(&mut self) {
        tokio::time::timeout(Duration::from_secs(10), self.authorized.recv())
            .await
            .expect("the core must acquire a slot and authorize the worker")
            .expect("the core authorization checkpoint must remain open");
    }

    async fn close(self) {
        self.worker.iroh.close().await;
        self.endpoint.close().await;
        self.router.shutdown().await.unwrap();
    }
}

async fn ci_fixture() -> CiFixture {
    let database = native::PostgresDatabase::new().await;
    let pool = database.pool.clone();
    let root = tempfile::tempdir().unwrap();
    let core_key = cluster::NodeKey::generate();
    let core_node = db::cluster::register_own_node(
        &pool,
        &core_key.public_hex(),
        "ci-core",
        &["storage".into()],
        "test",
    )
    .await
    .unwrap();
    let endpoint = cluster::bind(
        &cluster::Listen {
            bind: "127.0.0.1:0".parse().unwrap(),
            relay: cluster::Relay::Off,
            advertise: vec![],
        },
        &core_key,
        pool.clone(),
    )
    .await
    .unwrap();
    let context = NodeContext {
        pool: Some(pool.clone()),
        iroh: cluster::IrohDialer::with_endpoint(
            cluster::Relay::Off,
            core_key.clone(),
            endpoint.clone(),
        ),
        key: core_key.clone(),
        node_id: core_node.id,
        repos_path: root.path().to_owned(),
        peers: Default::default(),
        unreachable: Default::default(),
    };
    let owner = db::users::accounts::create_user(&pool, "ci_owner", None)
        .await
        .unwrap();
    let queue = RunQueue::Database {
        pool: pool.clone(),
        config: Arc::new(CiConfig {
            trusted_actor_ids: BTreeSet::from([owner.id]),
            ..CiConfig::default()
        }),
        secrets: None,
        context: Some(context.clone()),
        notifications: Arc::new(NotificationBroadcaster::new(&pool)),
    };
    let requests = Arc::new(AtomicUsize::new(0));
    let (authorized, authorization) = mpsc::unbounded_channel();
    let dispatch =
        tracing::Dispatch::new(tracing_subscriber::registry().with(CiAuthorization(authorized)));
    let router = cluster::router(endpoint.clone(), cluster::PairingHandler::new(pool.clone()))
        .accept(cluster::ALPN_RPC, rpc::RpcHandler::new(context.clone()))
        .accept(
            cluster::ALPN_EVENTS,
            CountingCi {
                inner: cluster::ci::CiHandler::new(context.clone(), queue),
                requests: requests.clone(),
                dispatch,
            },
        )
        .spawn();
    cluster::wait_for_address(&endpoint).await;
    db::cluster::record_node_heartbeat(
        &pool,
        &core_key.public_hex(),
        "test",
        &serde_json::to_string(&cluster::NodeAddress::of(&endpoint.addr())).unwrap(),
        Some(1),
    )
    .await
    .unwrap();

    let uuid = Uuid::new_v4();
    let path = root.path().join(format!("{uuid}.git"));
    let store = RepoStore::from_disk_path(path.to_str().unwrap());
    store.initialize(None, "main").await.unwrap();
    let repo = db::repositories::records::create_repository(
        &pool,
        owner.id,
        "project",
        None,
        true,
        path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let workflow = b"name: test\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo test\n";
    let revision = store
        .save_file_as(
            &pool,
            repo.id,
            owner.id,
            &codeyard::backend::repositories::store::FileWrite {
                branch: "main".into(),
                path: ".github/workflows/ci.yml".into(),
                message: "Add CI".into(),
                expected_revision: "unborn".into(),
                creating: true,
                deleting: false,
            },
            workflow,
            "Tester",
            "tester@example.invalid",
        )
        .await
        .unwrap();
    let blob = git2::Oid::hash_object(git2::ObjectType::Blob, workflow)
        .unwrap()
        .to_string();

    let worker_key = cluster::NodeKey::generate();
    db::cluster::upsert_declared_nodes(
        &pool,
        &[db::cluster::DeclaredMember {
            public_key: worker_key.public_hex(),
            name: "ci-worker".into(),
            roles: vec!["actions-worker".into()],
        }],
    )
    .await
    .unwrap();
    let worker = client(&worker_key).await;
    let peer = cluster::CorePeer {
        public_key: core_key.public_hex(),
        address: cluster::NodeAddress::of(&endpoint.addr()),
    };
    CiFixture {
        database,
        root,
        endpoint,
        router,
        requests,
        authorized: authorization,
        worker,
        peer,
        owner: owner.id,
        repo: repo.id,
        sha: revision.sha,
        blob,
    }
}

#[tokio::test]
async fn a_claim_frame_that_arrives_after_its_worker_is_revoked_is_refused() {
    let mut fixture = ci_fixture().await;
    let pool = fixture.pool().clone();
    let queued = fixture.queue_run().await;
    let connection = fixture
        .worker
        .iroh
        .endpoint()
        .await
        .unwrap()
        .connect(fixture.peer.endpoint_addr().unwrap(), cluster::ALPN_EVENTS)
        .await
        .unwrap();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    let frame =
        serde_json::to_vec(&json!({"worker": "slot-1", "operation": {"Claim": {"mode": "local"}}}))
            .unwrap();
    // Keep the body back until the core has checked membership and we have revoked it.
    send.write_u32(frame.len() as u32).await.unwrap();
    fixture.authorized().await;
    cluster::revoke(&pool, db::cluster::Authority::Operator, "ci-worker")
        .await
        .unwrap();
    send.write_all(&frame).await.unwrap();
    send.finish().unwrap();
    let reply: serde_json::Value =
        serde_json::from_slice(&rpc::read_frame(&mut recv, rpc::MAX_JSON).await.unwrap()).unwrap();
    assert_eq!(
        reply["result"]["Err"], "not an active actions-worker",
        "{reply}"
    );
    let run = db::ci::get_ci_run_by_run_id(&pool, queued.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, "queued");
    connection.close(0u32.into(), b"test complete");

    cluster::approve(&pool, db::cluster::Authority::Operator, "ci-worker")
        .await
        .unwrap();
    let claimed = fixture
        .remote()
        .claim("slot-1", CiExecutionMode::Local)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.run.id, queued.id);
    fixture.close().await;
}

#[tokio::test]
async fn a_core_that_is_not_active_refuses_ci_requests() {
    let fixture = ci_fixture().await;
    let pool = fixture.pool().clone();
    let queued = fixture.queue_run().await;
    for status in ["pending", "revoked"] {
        db::cluster::set_node_status(&pool, db::cluster::Authority::Operator, "ci-core", status)
            .await
            .unwrap();
        let Err(error) = fixture
            .remote()
            .claim("slot-1", CiExecutionMode::Local)
            .await
        else {
            panic!("a {status} core answered a claim");
        };
        assert!(
            error.to_string().contains("core node unavailable"),
            "{status}: {error:#}"
        );
        let run = db::ci::get_ci_run_by_run_id(&pool, queued.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, "queued", "{status}");
    }
    db::cluster::set_node_status(&pool, db::cluster::Authority::Operator, "ci-core", "active")
        .await
        .unwrap();
    let claimed = fixture
        .remote()
        .claim("slot-1", CiExecutionMode::Local)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.run.id, queued.id);
    fixture.close().await;
}

#[tokio::test]
async fn a_busy_core_refuses_a_request_at_once_when_all_slots_are_taken() {
    let mut fixture = ci_fixture().await;
    let heartbeat = json!({"worker": "slot-1", "operation": {"Heartbeat": {"id": 1}}});
    let body = serde_json::to_vec(&heartbeat).unwrap();
    // Sixteen requests take every slot: each sends the length of its frame and keeps the body back.
    let mut held = Vec::new();
    for _ in 0..16 {
        let connection = fixture
            .worker
            .iroh
            .endpoint()
            .await
            .unwrap()
            .connect(fixture.peer.endpoint_addr().unwrap(), cluster::ALPN_EVENTS)
            .await
            .unwrap();
        let (mut send, recv) = connection.open_bi().await.unwrap();
        send.write_u32(body.len() as u32).await.unwrap();
        fixture.authorized().await;
        held.push((connection, send, recv));
    }

    let connection = fixture
        .worker
        .iroh
        .endpoint()
        .await
        .unwrap()
        .connect(fixture.peer.endpoint_addr().unwrap(), cluster::ALPN_EVENTS)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let reply = stream_request(&connection, heartbeat.clone()).await;
    assert_eq!(reply["result"]["Err"], "core node unavailable", "{reply}");
    assert!(started.elapsed() < Duration::from_secs(5), "{reply}");
    connection.close(0u32.into(), b"test complete");

    for (connection, mut send, mut recv) in held {
        send.write_all(&body).await.unwrap();
        send.finish().unwrap();
        let reply: serde_json::Value =
            serde_json::from_slice(&rpc::read_frame(&mut recv, rpc::MAX_JSON).await.unwrap())
                .unwrap();
        assert!(reply["result"].get("Ok").is_some(), "{reply}");
        connection.close(0u32.into(), b"test complete");
    }
    let connection = fixture
        .worker
        .iroh
        .endpoint()
        .await
        .unwrap()
        .connect(fixture.peer.endpoint_addr().unwrap(), cluster::ALPN_EVENTS)
        .await
        .unwrap();
    let reply = stream_request(&connection, heartbeat).await;
    assert!(reply["result"].get("Ok").is_some(), "{reply}");
    connection.close(0u32.into(), b"test complete");
    fixture.close().await;
}

#[tokio::test]
async fn stalled_ci_replies_keep_their_admission_slots() {
    let fixture = ci_fixture().await;
    let key = iroh::SecretKey::generate();
    db::cluster::register_own_node(
        fixture.pool(),
        &key.public().to_string(),
        "slow-worker",
        &["actions-worker".into()],
        "test",
    )
    .await
    .unwrap();
    let slow = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .secret_key(key)
        .relay_mode(iroh::RelayMode::Disabled)
        .transport_config(
            iroh::endpoint::QuicTransportConfig::builder()
                .stream_receive_window(64u32.into())
                .build(),
        )
        .bind()
        .await
        .unwrap();
    let mut held = Vec::new();
    for _ in 0..16 {
        let queued = fixture.queue_run().await;
        let connection = slow
            .connect(fixture.peer.endpoint_addr().unwrap(), cluster::ALPN_EVENTS)
            .await
            .unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        rpc::write_frame(
            &mut send,
            &serde_json::to_vec(
                &json!({"worker": "slot-1", "operation": {"Claim": {"mode": "local"}}}),
            )
            .unwrap(),
            rpc::MAX_JSON,
        )
        .await
        .unwrap();
        send.finish().unwrap();
        // Seeing the length proves the claim has returned. Withhold credit for its body.
        let length = tokio::time::timeout(Duration::from_secs(5), recv.read_u32())
            .await
            .unwrap()
            .unwrap();
        assert!(length > 128);
        held.push((connection, recv, queued.id));
    }
    let queued = fixture.queue_run().await;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.remote().claim("slot-2", CiExecutionMode::Local),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(error) if error.to_string() == "core node unavailable"));
    assert_eq!(
        db::ci::get_ci_run_by_run_id(fixture.pool(), queued.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "queued"
    );
    for (connection, mut recv, id) in held {
        let body = tokio::time::timeout(Duration::from_secs(5), recv.read_to_end(rpc::MAX_JSON))
            .await
            .unwrap()
            .unwrap();
        let reply: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(reply["result"]["Ok"]["run"]["id"], id, "{reply}");
        connection.close(0u32.into(), b"test complete");
    }
    let claim = fixture
        .remote()
        .claim("slot-2", CiExecutionMode::Local)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.run.id, queued.id);
    slow.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn a_remote_pull_request_run_keeps_its_enqueued_event_after_the_base_moves() {
    let fixture = ci_fixture().await;
    let pool = fixture.pool();
    let repo = db::repositories::records::get_repository_by_id(pool, fixture.repo)
        .await
        .unwrap()
        .unwrap();
    let store = codeyard::backend::repositories::store::repo_store(&repo, None);
    {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        git.reference(
            "refs/heads/topic",
            git2::Oid::from_str(&fixture.sha).unwrap(),
            false,
            "topic",
        )
        .unwrap();
    }
    let head = store.save_file_as(pool, repo.id, fixture.owner, &codeyard::backend::repositories::store::FileWrite {
        branch: "topic".into(), path: ".github/workflows/ci.yml".into(), message: "Test pull".into(),
        expected_revision: fixture.sha.clone(), creating: false, deleting: false,
    }, b"on: pull_request\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n", "Tester", "tester@example.invalid").await.unwrap();
    let pull = db::pull_requests::records::create_pull_request(
        pool,
        db::pull_requests::records::NewPullRequest {
            repo_id: repo.id,
            head_repo_id: Some(repo.id),
            author_id: fixture.owner,
            title: "Original title",
            body: "",
            head_branch: "topic",
            head_sha: &head.sha,
            base_branch: "main",
            base_sha: &fixture.sha,
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let ci = codeyard::backend::ci::CiRuns::new(
        pool.clone(),
        Arc::new(CiConfig {
            trusted_actor_ids: BTreeSet::from([fixture.owner]),
            ..CiConfig::default()
        }),
        Arc::new(tokio::sync::Notify::new()),
        None,
        Arc::new(NotificationBroadcaster::new(pool)),
    );
    codeyard::backend::repositories::pull_requests::workflows::queue_pull_request_ci(
        pool,
        &ci,
        &repo,
        &pull,
        fixture.owner,
    )
    .await
    .unwrap();
    let run = db::ci::list_ci_runs_for_repo(pool, repo.id, 10)
        .await
        .unwrap()
        .pop()
        .unwrap();
    db::pull_requests::records::update_pull_request(
        pool,
        pull.id,
        db::discussions::DiscussionEdit {
            actor_id: fixture.owner,
            expected_state: "open",
            expected_updated: pull.updated_at,
            title: Some("Later title"),
            body: None,
            state: None,
        },
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let base = store
        .save_file_as(
            pool,
            repo.id,
            fixture.owner,
            &codeyard::backend::repositories::store::FileWrite {
                branch: "main".into(),
                path: "base.txt".into(),
                message: "Advance base".into(),
                expected_revision: fixture.sha.clone(),
                creating: true,
                deleting: false,
            },
            b"new base",
            "Tester",
            "tester@example.invalid",
        )
        .await
        .unwrap();
    codeyard::backend::repositories::pull_requests::branches_moved(pool, &repo, &ci, fixture.owner)
        .await;
    let current = db::pull_requests::records::get_pull_request_by_id(pool, pull.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.base_sha, base.sha);
    assert_eq!(current.head_sha, head.sha);
    let runner = fixture.root.path().join("runner");
    std::fs::write(&runner, format!(r#"#!/usr/bin/env python3
import json, subprocess, sys
args = sys.argv[1:]
with open(args[args.index('--eventpath') + 1]) as source:
    event = json.load(source)
pull = event['pull_request']
assert pull['title'] == 'Original title'
assert pull['head'] == {{'ref': 'topic', 'sha': '{head}'}}
assert pull['base'] == {{'ref': 'main', 'sha': '{old_base}'}}
repo = args[args.index('--directory') + 1]
subprocess.run(['git', '-C', repo, 'cat-file', '-e', pull['base']['sha']], check=True)
assert subprocess.run(['git', '-C', repo, 'cat-file', '-e', '{new_base}'], stderr=subprocess.DEVNULL).returncode != 0
print('CI_PR_SNAPSHOT_PASSED')
"#, head = head.sha, old_base = fixture.sha, new_base = base.sha)).unwrap();
    std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    let workers = CiWorkers::new(
        fixture.remote(),
        Arc::new(CiConfig {
            mode: CiExecutionMode::Local,
            act_binary: runner.to_str().unwrap().into(),
            max_active_runs: 1,
            run_timeout_seconds: 30,
            ..CiConfig::default()
        }),
        true,
        fixture.root.path(),
    );
    workers.startup().await.unwrap();
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let current = db::ci::get_ci_run_by_run_id(pool, run.run_id)
                .await
                .unwrap()
                .unwrap();
            if current.completed_at.is_some() {
                assert_eq!(current.status, "success", "{:?}", current.error_message);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    workers.shutdown().await.unwrap();
    assert!(db::ci::list_ci_logs(pool, run.id, 100)
        .await
        .unwrap()
        .iter()
        .any(|line| line.line == "CI_PR_SNAPSHOT_PASSED"));
    fixture.close().await;
}

#[tokio::test]
async fn a_runners_output_reaches_the_core_in_ordered_batches() {
    let fixture = ci_fixture().await;
    let pool = fixture.pool().clone();
    let runner = fixture.root.path().join("runner");
    std::fs::write(
        &runner,
        "#!/bin/sh\nset -eu\ni=1\nwhile test $i -le 1000; do\n  if test $i -eq 500; then printf 'line 500\\000binary\\001\\n'; else echo \"line $i\"; fi\n  i=$((i + 1))\ndone\n",
    )
    .unwrap();
    std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    let run = fixture.queue_run().await;
    let workers = CiWorkers::new(
        fixture.remote(),
        Arc::new(CiConfig {
            mode: CiExecutionMode::Local,
            act_binary: runner.to_str().unwrap().into(),
            max_active_runs: 1,
            run_timeout_seconds: 60,
            ..CiConfig::default()
        }),
        true,
        fixture.root.path(),
    );
    workers.startup().await.unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let current = db::ci::get_ci_run_by_run_id(&pool, run.run_id)
                .await
                .unwrap()
                .unwrap();
            if current.completed_at.is_some() {
                assert_eq!(current.status, "success", "{:?}", current.error_message);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    workers.shutdown().await.unwrap();

    let lines: Vec<String> = db::ci::list_ci_logs(&pool, run.id, 5000)
        .await
        .unwrap()
        .into_iter()
        .map(|line| line.line)
        .filter(|line| line.starts_with("line "))
        .collect();
    let expected: Vec<String> = (1..=1000)
        .map(|n| {
            if n == 500 {
                "line 500?binary?".to_owned()
            } else {
                format!("line {n}")
            }
        })
        .collect();
    assert_eq!(lines, expected);
    // One claim, the heartbeats, the batches and the final state: a few dozen requests in all,
    // where one request per line would need more than a thousand.
    let requests = fixture.requests.load(Ordering::SeqCst);
    assert!(requests < 100, "{requests} CI requests for 1000 lines");
    fixture.close().await;
}

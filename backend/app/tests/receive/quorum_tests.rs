use base64::Engine;
use codeyard_ci::runs::CiRuns;
use codeyard_database::models::RefValue;
use codeyard_database::DbPool;
use codeyard_git::error::RepoError;
use codeyard_git::protocol::RefUpdate;
use codeyard_git::repository::process as git_process;
use codeyard_git_storage::context::NodeContext;
use codeyard_git_storage::repositories::git_sessions::broker::{Gateway, HookState, Operation};
use codeyard_git_storage::repositories::git_sessions::private_files;
use codeyard_git_storage::repositories::git_sessions::protocol::TransactionPhase;
use codeyard_git_storage::repositories::git_sessions::ReceiveCredential;
use codeyard_nodes::transport::IrohDialer;
use codeyard_nodes::transport::NodeAddress;
use codeyard_nodes::transport::Relay;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_security::node_identity::NodeKey;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tokio::time::Duration;

fn commit(repo: &git2::Repository, message: &str) -> String {
    let tree = repo
        .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let author = git2::Signature::now("Writer", "writer@example.invalid").unwrap();
    repo.commit(None, &author, &author, message, &tree, &[])
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn prepared_hook_waits_for_a_decision_and_missing_voter_refuses_without_moving_refs() {
    let directory = tempfile::tempdir().unwrap();
    let native = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(directory.path().join("fixture.sqlite"))
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .map(codeyard_database::pool::Pool::from)
        .unwrap();
    let pool = DbPool::from(native.clone());
    codeyard_database::run_migrations(&pool).await.unwrap();
    codeyard_db_placement::node_config::upsert_node_configuration(&pool, "public", true)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE node_configuration SET write_quorum='2',write_timeout_seconds=5 WHERE id=1",
    )
    .execute(&native)
    .await
    .unwrap();
    let key = NodeKey::generate();
    let primary = codeyard_db_placement::cluster::register_own_node(
        &pool,
        &key.public_hex(),
        "primary",
        &["storage".into()],
        "test",
    )
    .await
    .unwrap()
    .id;
    let follower_key = NodeKey::generate();
    let follower = codeyard_db_placement::cluster::register_own_node(
        &pool,
        &follower_key.public_hex(),
        "follower",
        &["storage".into()],
        "test",
    )
    .await
    .unwrap()
    .id;
    // Accept TCP but never finish TLS: preparation must wait, not invent a vote.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = NodeAddress {
        tcp: vec![listener.local_addr().unwrap()],
        ..Default::default()
    };
    codeyard_db_placement::cluster::record_node_heartbeat(
        &pool,
        &follower_key.public_hex(),
        "test",
        &serde_json::to_string(&address).unwrap(),
        None,
    )
    .await
    .unwrap();
    let owner = codeyard_db_identity::users::accounts::create_user(&pool, "owner", None)
        .await
        .unwrap();
    let mut wire = Vec::from(&b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..]);
    wire.extend_from_slice(&[7; 32]);
    let public_key = format!(
        "ssh-ed25519 {}",
        base64::engine::general_purpose::STANDARD.encode(wire)
    );
    let credential =
        codeyard_db_identity::users::ssh_keys::add_ssh_key(&pool, owner.id, "owner", &public_key)
            .await
            .unwrap();
    let root = directory.path().join("repos");
    std::fs::create_dir(&root).unwrap();
    let path = codeyard_git_storage::context::term_path(&root, uuid::Uuid::new_v4(), 1);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let repository = git2::Repository::init_bare(&path).unwrap();
    let old = commit(&repository, "old");
    let new = commit(&repository, "new");
    repository
        .reference(
            "refs/heads/main",
            git2::Oid::from_str(&old).unwrap(),
            true,
            "initial",
        )
        .unwrap();
    repository.set_head("refs/heads/main").unwrap();
    let repo = codeyard_db_repos::repositories::records::create_repository(
        &pool,
        owner.id,
        "repo",
        None,
        false,
        path.to_str().unwrap(),
    )
    .await
    .unwrap();
    assert!(
        codeyard_db_placement::cluster::place_repository(&pool, repo.id, primary)
            .await
            .unwrap()
    );
    codeyard_db_placement::cluster::writes::bootstrap_writes(
        &pool,
        repo.id,
        primary,
        &[primary, follower],
        &[
            RefValue {
                name: "HEAD".into(),
                kind: "symbolic".into(),
                target: "refs/heads/main".into(),
            },
            RefValue {
                name: "refs/heads/main".into(),
                kind: "oid".into(),
                target: old.clone(),
            },
        ],
    )
    .await
    .unwrap();
    assert!(codeyard_db_placement::cluster::writes::mark_write_applied(
        &pool, repo.id, follower, 1, 0
    )
    .await
    .unwrap());
    assert!(
        codeyard_db_placement::cluster::writes::enable_write_family(&pool, &[(repo.id, 1)])
            .await
            .unwrap()
    );
    let context = NodeContext {
        pool: Some(pool.clone()),
        rpc: std::sync::Arc::new(codeyard_repos_rpc::RpcFactory),
        iroh: IrohDialer::new(Relay::Off, key.clone()),
        key,
        node_id: primary,
        repos_path: root.clone(),
        peers: Arc::new(RwLock::new(HashMap::new())),
        unreachable: Arc::new(Mutex::new(HashMap::new())),
        clone_cursor: Default::default(),
        ci_replicas: Arc::new(RwLock::new(HashMap::new())),
    };
    let ci = Arc::new(CiRuns::new(
        pool.clone(),
        Arc::new(codeyard_ci::config::CiConfig::default()),
        Arc::new(tokio::sync::Notify::new()),
        Some(context),
        Arc::new(NotificationBroadcaster::new(&pool)),
    ));
    let gateway = Arc::new(Gateway {
        socket: directory.path().join("broker.sock"),
        repos_root: root,
        executable: std::env::current_exe().unwrap(),
        git: git_process::executable().unwrap(),
        pool: pool.clone(),
        ci,
        git_user: "git".into(),
        uid: private_files::effective_uid(),
        sessions: git_process::session_slots(),
        controls: Semaphore::new(2),
        operations: Mutex::new(HashMap::new()),
    });
    let lease = codeyard_git::locks::open_publication(&path).await.unwrap();
    let publication = lease.acquire(&path).await.unwrap();
    let updates = vec![RefUpdate {
        old: old.clone(),
        new,
        reference: "refs/heads/main".into(),
    }];
    let operation = Arc::new(Operation {
        credential: ReceiveCredential::SshKey(credential.id),
        actor_id: owner.id,
        repo_id: repo.id,
        owner: "owner".into(),
        path: path.clone(),
        publication_lease: Some(lease),
        hooks: tempfile::tempdir().unwrap(),
        state: Mutex::new(HookState {
            proposed: Some(updates.clone()),
            publication: Some(publication),
            quorum: true,
            ..HookState::default()
        }),
    });
    let task_gateway = gateway.clone();
    let task_operation = operation.clone();
    let task = tokio::spawn(async move {
        task_gateway
            .reference_transaction(&task_operation, TransactionPhase::Prepared, &updates)
            .await
    });
    let (_connection, _) = timeout(Duration::from_secs(10), listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !task.is_finished(),
        "prepared hook must remain held without a decision"
    );
    let view = codeyard_db_placement::cluster::writes::recovery_view(&pool, repo.id)
        .await
        .unwrap();
    let pending = view.unfinished.unwrap();
    assert_eq!(pending.decision, "pending");
    assert_eq!(pending.votes.len(), 1);
    assert_eq!(pending.votes[0].node_id, primary);
    assert_eq!(
        repository
            .refname_to_id("refs/heads/main")
            .unwrap()
            .to_string(),
        old
    );
    let error = timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<RepoError>().unwrap().message,
        "write quorum unavailable"
    );
    let decided = codeyard_db_placement::cluster::writes::load_write(&pool, &pending.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decided.decision, "abort");
    assert_eq!(
        repository
            .refname_to_id("refs/heads/main")
            .unwrap()
            .to_string(),
        old
    );
    gateway.complete_quorum(&operation).await.unwrap();
}

//! Real receive hooks and two in-process storage RPC servers share one database.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use super::quorum_sql::{replace_node_key, set_timeout, setup_with_refs, Fixture};
use crate::support::native_database as native;
use base64::Engine;
use codeyard::backend::{
    ci::{queue::RunQueue, CiConfig, CiRuns},
    cluster::{
        self, context::term_path, rpc, tcp, IrohDialer, NodeAddress, NodeContext, PairingHandler,
        Relay,
    },
    db::{self, cluster::writes, models::RefValue, DbPool},
    notifications::NotificationBroadcaster,
    repositories::{
        git_sessions::{receive_session, ReceiveCredential, ReceiveSession},
        store::{GitService, GitSession, GitStreams, RepoStore},
    },
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::time::{timeout, Instant};

const MAIN: &str = "refs/heads/main";
const WAIT: Duration = Duration::from_secs(20);

fn commit(repo: &git2::Repository, message: &str, parent: Option<git2::Oid>) -> String {
    let tree = repo
        .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let signature = git2::Signature::now("Writer", "writer@example.test").unwrap();
    let parent = parent.map(|oid| repo.find_commit(oid).unwrap());
    let parents = parent.iter().collect::<Vec<_>>();
    repo.commit(None, &signature, &signature, message, &tree, &parents)
        .unwrap()
        .to_string()
}

struct Stores {
    fixture: Fixture,
    contexts: Vec<NodeContext>,
    listeners: Vec<tcp::Listener>,
    paths: Vec<PathBuf>,
    _roots: Vec<tempfile::TempDir>,
    old: String,
    new: String,
    credential: ReceiveCredential,
}

impl Stores {
    async fn new(pool: &DbPool) -> Self {
        let seed = tempfile::tempdir().unwrap();
        let seed_repo = git2::Repository::init_bare(seed.path()).unwrap();
        let old = commit(&seed_repo, "initial", None);
        seed_repo
            .reference(MAIN, git2::Oid::from_str(&old).unwrap(), true, "initial")
            .unwrap();
        seed_repo.set_head(MAIN).unwrap();
        let refs = [
            RefValue {
                name: "HEAD".into(),
                kind: "symbolic".into(),
                target: MAIN.into(),
            },
            RefValue {
                name: MAIN.into(),
                kind: "oid".into(),
                target: old.clone(),
            },
        ];
        let fixture = setup_with_refs(pool, 2, "2", &refs).await;
        let repository = db::repositories::records::get_repository_by_id(pool, fixture.repo)
            .await
            .unwrap()
            .unwrap();
        let uuid = rpc::repository_uuid(&repository.disk_path).unwrap();
        let mut roots = Vec::new();
        let mut paths = Vec::new();
        let mut contexts = Vec::new();
        let mut listeners = Vec::new();
        for node in &fixture.voters {
            let root = tempfile::tempdir().unwrap();
            let path = term_path(root.path(), uuid, 1);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            git2::build::RepoBuilder::new()
                .bare(true)
                .clone(seed.path().to_str().unwrap(), &path)
                .unwrap();
            let key = replace_node_key(pool, *node).await;
            let context = NodeContext {
                pool: Some(pool.clone()),
                iroh: IrohDialer::new(Relay::Off, key.clone()),
                key,
                node_id: *node,
                repos_path: root.path().to_owned(),
                peers: Default::default(),
                unreachable: Default::default(),
                ci_replicas: Default::default(),
            };
            let queue = RunQueue::Database {
                pool: pool.clone(),
                config: Arc::new(CiConfig::default()),
                secrets: None,
                context: Some(context.clone()),
                notifications: Arc::new(NotificationBroadcaster::new(pool)),
            };
            let listener = tcp::bind(
                "127.0.0.1:0".parse().unwrap(),
                &context.key,
                tcp::Services {
                    pool: pool.clone(),
                    pairing: PairingHandler::new(pool.clone()),
                    rpc: Some(rpc::RpcHandler::new(context.clone())),
                    events: cluster::ci::CiHandler::new(context.clone(), queue),
                },
            )
            .await
            .unwrap();
            let address = NodeAddress {
                tcp: vec![listener.addr()],
                ..Default::default()
            };
            db::cluster::record_node_heartbeat(
                pool,
                &context.key.public_hex(),
                "test",
                &serde_json::to_string(&address).unwrap(),
                Some(1024),
            )
            .await
            .unwrap();
            roots.push(root);
            paths.push(path);
            contexts.push(context);
            listeners.push(listener);
        }
        let primary = git2::Repository::open_bare(&paths[0]).unwrap();
        let new = commit(&primary, "pushed", Some(git2::Oid::from_str(&old).unwrap()));
        let mut key = Vec::from(&b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..]);
        key.extend_from_slice(&[7; 32]);
        let key = format!(
            "ssh-ed25519 {}",
            base64::engine::general_purpose::STANDARD.encode(key)
        );
        let credential = db::users::ssh_keys::add_ssh_key(pool, fixture.owner, "owner", &key)
            .await
            .unwrap();
        Self {
            fixture,
            contexts,
            listeners,
            paths,
            _roots: roots,
            old,
            new,
            credential: ReceiveCredential::SshKey(credential.id),
        }
    }

    async fn push(
        &self,
        pool: &DbPool,
    ) -> (anyhow::Result<std::process::ExitStatus>, Vec<u8>, Vec<u8>) {
        let primary = RepoStore::from_disk_path(self.paths[0].to_str().unwrap());
        let mut pack = Vec::new();
        let status = primary
            .replica_pack(
                std::slice::from_ref(&self.new),
                std::slice::from_ref(&self.old),
                GitStreams {
                    input: &mut tokio::io::empty(),
                    output: &mut pack,
                    errors: &mut tokio::io::sink(),
                },
            )
            .await
            .unwrap();
        assert!(status.success());
        let command = format!("{} {} {MAIN}\0report-status atomic\n", self.old, self.new);
        let mut input = format!("{:04x}{command}0000", command.len() + 4).into_bytes();
        input.extend_from_slice(&pack);
        let mut session = GitSession::new(GitService::ReceivePack, true);
        session.credential = Some(self.credential);
        session.owner = "quorum-owner".into();
        let ci = Arc::new(CiRuns::new(
            pool.clone(),
            Arc::new(CiConfig::default()),
            Arc::new(tokio::sync::Notify::new()),
            Some(self.contexts[0].clone()),
            Arc::new(NotificationBroadcaster::new(pool)),
        ));
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let status = timeout(
            WAIT,
            receive_session(
                pool.clone(),
                ci,
                ReceiveSession {
                    repo_id: self.fixture.repo,
                    path: &self.paths[0],
                    executable: PathBuf::from(env!("CARGO_BIN_EXE_codeyard")),
                    session,
                },
                GitStreams {
                    input: &mut input.as_slice(),
                    output: &mut output,
                    errors: &mut errors,
                },
            ),
        )
        .await
        .expect("receive deadline");
        (status, output, errors)
    }

    async fn refs(&self) -> Vec<std::collections::HashMap<String, String>> {
        let mut refs = Vec::new();
        for path in &self.paths {
            refs.push(
                RepoStore::from_disk_path(path.to_str().unwrap())
                    .mirror_refs()
                    .await
                    .unwrap(),
            );
        }
        refs
    }

    async fn rpc_refs(&self, pool: &DbPool) -> std::collections::HashMap<String, String> {
        let key = cluster::NodeKey::generate();
        let web =
            db::cluster::register_own_node(pool, &key.public_hex(), "web", &["web".into()], "test")
                .await
                .unwrap();
        let mut context = self.contexts[0].clone();
        context.key = key.clone();
        context.iroh = IrohDialer::new(Relay::Off, key);
        context.node_id = web.id;
        let repository = db::repositories::records::get_repository_by_id(pool, self.fixture.repo)
            .await
            .unwrap()
            .unwrap();
        let (reply, _) = context
            .call(
                self.fixture.primary,
                rpc::Request {
                    repository: rpc::repository_uuid(&repository.disk_path).unwrap(),
                    operation: rpc::Operation::Refs,
                    actor: None,
                    ci_run: None,
                },
                &[],
            )
            .await
            .unwrap();
        let refs = serde_json::from_value(reply.result.unwrap()).unwrap();
        context.iroh.close().await;
        refs
    }

    async fn stop(&self) {
        for listener in &self.listeners {
            listener.stop().await;
        }
        for context in &self.contexts {
            context.iroh.close().await;
        }
    }
}

async fn last_write(pool: &DbPool, repo: i64) -> db::models::WriteRecord {
    let query = "SELECT id FROM repository_writes WHERE repository_id=$1 ORDER BY base_version DESC LIMIT 1";
    let id: String = match pool {
        DbPool::Postgres(raw) => sqlx::query_scalar(query)
            .bind(repo)
            .fetch_one(raw)
            .await
            .unwrap(),
        DbPool::Sqlite(raw) => sqlx::query_scalar(query)
            .bind(repo)
            .fetch_one(raw)
            .await
            .unwrap(),
    };
    writes::load_write(pool, &id).await.unwrap().unwrap()
}

async fn both_voters_publish(pool: &DbPool) {
    let stores = Stores::new(pool).await;
    let (status, output, errors) = stores.push(pool).await;
    assert!(
        status.as_ref().is_ok_and(|status| status.success()),
        "{status:?}: {}",
        String::from_utf8_lossy(&errors)
    );
    let report = String::from_utf8_lossy(&output);
    assert!(
        report.contains("unpack ok") && report.contains("ok refs/heads/main"),
        "{report}"
    );
    assert!(!report.contains("ng refs/heads/main"));
    let refs = stores.refs().await;
    assert_eq!(refs[0], refs[1]);
    assert_eq!(refs[0][MAIN], stores.new);
    assert_eq!(stores.rpc_refs(pool).await[MAIN], stores.new);
    assert!(refs[0]
        .keys()
        .any(|name| name.starts_with("refs/codeyard/publications/")));
    assert!(!refs[0]
        .keys()
        .any(|name| name.starts_with("refs/codeyard/writes/")));
    let write = last_write(pool, stores.fixture.repo).await;
    assert_eq!(write.decision, "commit");
    assert_eq!(write.votes.len(), 2);
    assert!(write.finished);
    let view = writes::recovery_view(pool, stores.fixture.repo)
        .await
        .unwrap();
    assert_eq!(
        view.refs
            .iter()
            .find(|value| value.name == MAIN)
            .unwrap()
            .target,
        stores.new
    );
    assert!(view
        .voters
        .iter()
        .all(|voter| voter.applied_version == view.committed_version));
    assert!(view.unfinished.is_none());
    stores.stop().await;
}

async fn editor_uses_both_stores(pool: &DbPool) {
    let stores = Stores::new(pool).await;
    let f = &stores.fixture;
    let row = db::repositories::records::get_repository_by_id(pool, f.repo)
        .await
        .unwrap()
        .unwrap();
    let store = stores.contexts[0]
        .repository_store(&row, false)
        .await
        .unwrap();
    let commit = store
        .save_file_as(
            pool,
            f.repo,
            f.owner,
            &codeyard::backend::repositories::store::FileWrite {
                branch: "main".into(),
                path: "README.md".into(),
                message: "save file".into(),
                expected_revision: stores.old.clone(),
                creating: true,
                deleting: false,
            },
            b"quorum editor\n",
            "Writer",
            "writer@example.test",
        )
        .await
        .unwrap();
    let refs = stores.refs().await;
    assert_eq!(refs[0], refs[1]);
    assert_eq!(refs[0][MAIN], commit.sha);
    let write = last_write(pool, f.repo).await;
    assert_eq!(write.votes.len(), 2);
    assert!(write.finished);
    for path in &stores.paths {
        let repo = git2::Repository::open_bare(path).unwrap();
        let commit = repo
            .find_commit(git2::Oid::from_str(&commit.sha).unwrap())
            .unwrap();
        let tree = commit.tree().unwrap();
        let blob = repo
            .find_blob(tree.get_name("README.md").unwrap().id())
            .unwrap();
        assert_eq!(blob.content(), b"quorum editor\n");
    }
    stores.stop().await;
}

async fn refusing_voter_keeps_refs(pool: &DbPool) {
    let stores = Stores::new(pool).await;
    let before = stores.refs().await;
    // A voter with no preparation storage cannot record a durable vote.
    tokio::fs::write(stores.paths[1].join("codeyard-writes"), b"not a directory")
        .await
        .unwrap();
    let (_, output, _) = stores.push(pool).await;
    assert!(!String::from_utf8_lossy(&output).contains("ok refs/heads/main"));
    assert_eq!(stores.refs().await, before);
    let write = last_write(pool, stores.fixture.repo).await;
    assert_eq!(write.decision, "abort");
    assert_eq!(write.votes.len(), 1);
    assert_eq!(write.votes[0].node_id, stores.fixture.primary);
    let view = writes::recovery_view(pool, stores.fixture.repo)
        .await
        .unwrap();
    assert_eq!(view.committed_version, 0);
    assert_eq!(
        view.refs
            .iter()
            .find(|value| value.name == MAIN)
            .unwrap()
            .target,
        stores.old
    );
    stores.stop().await;
}

async fn unreachable_voter_times_out(pool: &DbPool) {
    let stores = Stores::new(pool).await;
    set_timeout(pool, 1).await.unwrap();
    let before = stores.refs().await;
    let stalled = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = NodeAddress {
        tcp: vec![stalled.local_addr().unwrap()],
        ..Default::default()
    };
    db::cluster::record_node_heartbeat(
        pool,
        &stores.contexts[1].key.public_hex(),
        "test",
        &serde_json::to_string(&address).unwrap(),
        Some(1024),
    )
    .await
    .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (accepted, connected) = tokio::sync::oneshot::channel();
    let stalled = tokio::spawn(async move {
        let (connection, _) = stalled.accept().await.unwrap();
        accepted.send(()).unwrap();
        let _ = stopped.await;
        drop(connection);
    });
    let started = Instant::now();
    let (_, output, _) = stores.push(pool).await;
    timeout(Duration::from_secs(1), connected)
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(started.elapsed() < Duration::from_secs(6));
    stop.send(()).unwrap();
    timeout(Duration::from_secs(1), stalled)
        .await
        .unwrap()
        .unwrap();
    assert!(!String::from_utf8_lossy(&output).contains("ok refs/heads/main"));
    assert_eq!(stores.refs().await, before);
    assert_eq!(
        last_write(pool, stores.fixture.repo).await.decision,
        "abort"
    );
    assert_eq!(
        writes::recovery_view(pool, stores.fixture.repo)
            .await
            .unwrap()
            .committed_version,
        0
    );
    stores.stop().await;
}

async fn stale_writer_is_refused(pool: &DbPool) {
    let stores = Stores::new(pool).await;
    let f = &stores.fixture;
    let row = db::repositories::records::get_repository_by_id(pool, f.repo)
        .await
        .unwrap()
        .unwrap();
    let stale = stores.contexts[0]
        .repository_store(&row, false)
        .await
        .unwrap();
    assert_eq!(stale.refs().await.unwrap()[MAIN], stores.old);
    assert!(writes::switch_write_family(
        pool,
        f.repo,
        f.primary,
        &[(f.repo, 1)],
        f.primary,
        &f.voters
    )
    .await
    .unwrap());
    for context in &stores.contexts {
        cluster::replicas::recover_family(pool, f.repo, context.node_id, &context.repos_path)
            .await
            .unwrap();
    }
    assert!(writes::recovery_view(pool, f.repo).await.unwrap().serving);
    assert!(stale
        .create_branch_as(pool, f.repo, f.owner, "stale", &stores.old)
        .await
        .is_err());
    let (status, output, _) = stores.push(pool).await;
    assert!(status.is_err() || !status.unwrap().success());
    assert!(!String::from_utf8_lossy(&output).contains("ok refs/heads/main"));
    assert!(writes::recovery_view(pool, f.repo)
        .await
        .unwrap()
        .unfinished
        .is_none());
    let current = stores.contexts[0]
        .repository_store(&row, false)
        .await
        .unwrap();
    current
        .create_branch_as(pool, f.repo, f.owner, "current", &stores.old)
        .await
        .unwrap();
    let uuid = rpc::repository_uuid(&row.disk_path).unwrap();
    for context in &stores.contexts {
        let repo = git2::Repository::open_bare(term_path(&context.repos_path, uuid, 2)).unwrap();
        assert!(repo.find_reference("refs/heads/stale").is_err());
        assert_eq!(
            repo.refname_to_id("refs/heads/current")
                .unwrap()
                .to_string(),
            stores.old
        );
    }
    assert!(stores
        .refs()
        .await
        .iter()
        .all(|refs| !refs.contains_key("refs/heads/current")));
    assert_eq!(
        stores.rpc_refs(pool).await["refs/heads/current"],
        stores.old
    );
    stores.stop().await;
}

macro_rules! engine_pair {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[tokio::test]
        async fn $sqlite() {
            let database = native::NativeDatabase::new().await;
            $contract(&database.pool).await;
        }
        #[tokio::test]
        async fn $postgres() {
            let mut database = native::PostgresDatabase::new().await;
            $contract(&database.pool).await;
            database.finish().await;
        }
    };
}
engine_pair!(
    sqlite_both_voters_publish,
    postgres_both_voters_publish,
    both_voters_publish
);
engine_pair!(
    sqlite_editor_uses_both_stores,
    postgres_editor_uses_both_stores,
    editor_uses_both_stores
);
engine_pair!(
    sqlite_refusing_voter_keeps_refs,
    postgres_refusing_voter_keeps_refs,
    refusing_voter_keeps_refs
);
engine_pair!(
    sqlite_unreachable_voter_times_out,
    postgres_unreachable_voter_times_out,
    unreachable_voter_times_out
);
engine_pair!(
    sqlite_stale_writer_is_refused,
    postgres_stale_writer_is_refused,
    stale_writer_is_refused
);

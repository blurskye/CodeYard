//! Real receive hooks and two in-process storage RPC servers share one database.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use super::quorum_sql::{replace_node_key, set_timeout, setup_with_refs, Fixture};
use crate::support::native_database as native;
use base64::Engine;
use codeyard_ci::config::CiConfig;
use codeyard_ci_engine::queue::RunQueue;
use codeyard_ci_engine::runs::CiRuns;

use codeyard_repos_core::context::term_path;

use codeyard_cluster_core::pairing::PairingHandler;
use codeyard_cluster_core::transport::IrohDialer;
use codeyard_cluster_core::transport::NodeAddress;
use codeyard_cluster_core::transport::Relay;
use codeyard_repos_core::context::NodeContext;

use codeyard_db_core::models::RefValue;
use codeyard_db_core::DbPool;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_repos_core::repositories::git_sessions::receive_session;
use codeyard_repos_core::repositories::git_sessions::ReceiveCredential;
use codeyard_repos_core::repositories::git_sessions::ReceiveSession;
use codeyard_repos_core::repositories::store::RepoStore;
use codeyard_repos_core::store_types::GitService;
use codeyard_repos_core::store_types::GitSession;
use codeyard_repos_core::store_types::GitStreams;
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

pub(super) struct Stores {
    pub(super) fixture: Fixture,
    pub(super) contexts: Vec<NodeContext>,
    listeners: Vec<codeyard_cluster::tcp::Listener>,
    paths: Vec<PathBuf>,
    _roots: Vec<tempfile::TempDir>,
    pub(super) old: String,
    new: String,
    credential: ReceiveCredential,
}

impl Stores {
    pub(super) async fn new(pool: &DbPool) -> Self {
        Self::with_voters(pool, 2).await
    }

    async fn with_voters(pool: &DbPool, count: usize) -> Self {
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
        let fixture = setup_with_refs(pool, count, "majority", &refs).await;
        assert!(crate::support::native_database::enable_cluster(pool)
            .await
            .unwrap());
        let repository =
            codeyard_db_repos::repositories::records::get_repository_by_id(pool, fixture.repo)
                .await
                .unwrap()
                .unwrap();
        let uuid = codeyard_repos_core::wire::repository_uuid(&repository.disk_path).unwrap();
        let mut roots = Vec::new();
        let mut paths = Vec::new();
        let mut contexts = Vec::new();
        let mut listeners = Vec::new();
        for node in &fixture.voters {
            let root = tempfile::tempdir().unwrap();
            let path = term_path(root.path(), uuid, 1);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let clone = git2::build::RepoBuilder::new()
                .bare(true)
                .clone(seed.path().to_str().unwrap(), &path)
                .unwrap();
            // A voter holds exactly the committed refs, not the clone's remote-tracking refs.
            let remotes: Vec<String> = clone
                .references_glob("refs/remotes/*")
                .unwrap()
                .map(|reference| reference.unwrap().name().unwrap().to_owned())
                .collect();
            for name in remotes {
                clone.find_reference(&name).unwrap().delete().unwrap();
            }
            let key = replace_node_key(pool, *node).await;
            let context = NodeContext {
                pool: Some(pool.clone()),
                rpc: std::sync::Arc::new(codeyard_repos_rpc::RpcFactory),
                iroh: IrohDialer::new(Relay::Off, key.clone()),
                key,
                node_id: *node,
                repos_path: root.path().to_owned(),
                peers: Default::default(),
                unreachable: Default::default(),
                clone_cursor: Default::default(),
                ci_replicas: Default::default(),
            };
            let queue = RunQueue::Database {
                pool: pool.clone(),
                config: Arc::new(CiConfig::default()),
                secrets: None,
                context: Some(context.clone()),
                notifications: Arc::new(NotificationBroadcaster::new(pool)),
            };
            let listener = codeyard_cluster::tcp::bind(
                "127.0.0.1:0".parse().unwrap(),
                &context.key,
                codeyard_cluster::tcp::Services {
                    pool: pool.clone(),
                    pairing: PairingHandler::new(pool.clone()),
                    rpc: Some(codeyard_repos_rpc::RpcHandler::new(context.clone())),
                    events: codeyard_cluster::ci::CiHandler::new(context.clone(), queue),
                },
            )
            .await
            .unwrap();
            let address = NodeAddress {
                tcp: vec![listener.addr()],
                ..Default::default()
            };
            codeyard_db_cluster::cluster::record_node_heartbeat(
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
        let credential =
            codeyard_db_identity::users::ssh_keys::add_ssh_key(pool, fixture.owner, "owner", &key)
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

    pub(super) async fn push(
        &self,
        pool: &DbPool,
    ) -> (anyhow::Result<std::process::ExitStatus>, Vec<u8>, Vec<u8>) {
        self.push_with_input(pool, self.push_input().await).await
    }

    pub(super) async fn push_input(&self) -> Vec<u8> {
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
        input
    }

    pub(super) async fn push_with_input(
        &self,
        pool: &DbPool,
        input: Vec<u8>,
    ) -> (anyhow::Result<std::process::ExitStatus>, Vec<u8>, Vec<u8>) {
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

    pub(super) async fn refs(&self) -> Vec<std::collections::HashMap<String, String>> {
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
        let key = codeyard_support::node_identity::NodeKey::generate();
        let web = codeyard_db_cluster::cluster::register_own_node(
            pool,
            &key.public_hex(),
            "web",
            &["web".into()],
            "test",
        )
        .await
        .unwrap();
        let mut context = self.contexts[0].clone();
        context.key = key.clone();
        context.iroh = IrohDialer::new(Relay::Off, key);
        context.node_id = web.id;
        let repository =
            codeyard_db_repos::repositories::records::get_repository_by_id(pool, self.fixture.repo)
                .await
                .unwrap()
                .unwrap();
        let (reply, _) = context
            .call(
                self.fixture.primary,
                codeyard_repos_core::wire::Request {
                    repository: codeyard_repos_core::wire::repository_uuid(&repository.disk_path)
                        .unwrap(),
                    operation: codeyard_repos_core::wire::Operation::Refs,
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

    async fn store(&self, pool: &DbPool) -> RepoStore {
        let row =
            codeyard_db_repos::repositories::records::get_repository_by_id(pool, self.fixture.repo)
                .await
                .unwrap()
                .unwrap();
        self.contexts[0]
            .repository_store(&row, false)
            .await
            .unwrap()
    }

    fn heads(&self) -> Vec<String> {
        self.paths
            .iter()
            .map(|path| {
                git2::Repository::open_bare(path)
                    .unwrap()
                    .find_reference("HEAD")
                    .unwrap()
                    .symbolic_target()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    pub(super) async fn stop(&self) {
        for listener in &self.listeners {
            listener.stop().await;
        }
        for context in &self.contexts {
            context.iroh.close().await;
        }
    }
}

async fn last_write(pool: &DbPool, repo: i64) -> codeyard_db_core::models::WriteRecord {
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
    codeyard_db_cluster::cluster::writes::load_write(pool, &id)
        .await
        .unwrap()
        .unwrap()
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
    let view = codeyard_db_cluster::cluster::writes::recovery_view(pool, stores.fixture.repo)
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
    let row = codeyard_db_repos::repositories::records::get_repository_by_id(pool, f.repo)
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
            &codeyard_repos::editor::FileWrite {
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
    let view = codeyard_db_cluster::cluster::writes::recovery_view(pool, stores.fixture.repo)
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
    codeyard_db_cluster::cluster::record_node_heartbeat(
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
        codeyard_db_cluster::cluster::writes::recovery_view(pool, stores.fixture.repo)
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
    let row = codeyard_db_repos::repositories::records::get_repository_by_id(pool, f.repo)
        .await
        .unwrap()
        .unwrap();
    let stale = stores.contexts[0]
        .repository_store(&row, false)
        .await
        .unwrap();
    assert_eq!(stale.refs().await.unwrap()[MAIN], stores.old);
    assert!(codeyard_db_cluster::cluster::writes::switch_write_family(
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
        codeyard_cluster::replicas::promote::recover_family(
            pool,
            f.repo,
            context.node_id,
            &context.repos_path,
        )
        .await
        .unwrap();
    }
    assert!(
        codeyard_db_cluster::cluster::writes::recovery_view(pool, f.repo)
            .await
            .unwrap()
            .serving
    );
    assert!(stale
        .create_branch_as(pool, f.repo, f.owner, "stale", &stores.old)
        .await
        .is_err());
    let (status, output, _) = stores.push(pool).await;
    assert!(status.is_err() || !status.unwrap().success());
    assert!(!String::from_utf8_lossy(&output).contains("ok refs/heads/main"));
    assert!(
        codeyard_db_cluster::cluster::writes::recovery_view(pool, f.repo)
            .await
            .unwrap()
            .unfinished
            .is_none()
    );
    let current = stores.contexts[0]
        .repository_store(&row, false)
        .await
        .unwrap();
    current
        .create_branch_as(pool, f.repo, f.owner, "current", &stores.old)
        .await
        .unwrap();
    let uuid = codeyard_repos_core::wire::repository_uuid(&row.disk_path).unwrap();
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

async fn rename_is_one_write(pool: &DbPool) {
    let stores = Stores::new(pool).await;
    let f = &stores.fixture;
    let store = stores.store(pool).await;
    let renamed = "refs/heads/renamed";
    assert_eq!(
        store
            .rename_branch_as(pool, f.repo, f.owner, "main", "renamed")
            .await
            .unwrap(),
        stores.old
    );
    let refs = stores.refs().await;
    assert_eq!(refs[0], refs[1]);
    assert!(!refs[0].contains_key(MAIN));
    assert_eq!(refs[0][renamed], stores.old);
    assert_eq!(stores.heads(), vec![renamed; 2]);
    let write = last_write(pool, f.repo).await;
    assert!(write.finished);
    assert_eq!(write.votes.len(), 2);
    assert_eq!(write.base_version, 0);
    for reference in [MAIN, renamed, "HEAD"] {
        assert!(write.changes.iter().any(|change| change.name == reference));
    }
    assert!(write
        .changes
        .iter()
        .any(|change| change.name.starts_with("refs/codeyard/publications/")));
    assert_eq!(
        codeyard_db_repos::repositories::records::get_repository_by_id(pool, f.repo)
            .await
            .unwrap()
            .unwrap()
            .default_branch,
        "renamed"
    );
    assert_eq!(
        codeyard_db_repos::repositories::protection::branch_redirect(pool, f.repo, "main")
            .await
            .unwrap()
            .as_deref(),
        Some("renamed")
    );

    stores.listeners[1].stop().await;
    set_timeout(pool, 1).await.unwrap();
    let before = codeyard_db_cluster::cluster::writes::recovery_view(pool, f.repo)
        .await
        .unwrap();
    let error = timeout(
        WAIT,
        store.rename_branch_as(pool, f.repo, f.owner, "renamed", "failed"),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.message, "write quorum unavailable");
    assert_eq!(stores.refs().await, refs);
    assert_eq!(stores.heads(), vec![renamed; 2]);
    let write = last_write(pool, f.repo).await;
    assert_eq!(write.decision, "abort");
    assert_eq!(write.votes.len(), 1);
    let after = codeyard_db_cluster::cluster::writes::recovery_view(pool, f.repo)
        .await
        .unwrap();
    assert_eq!(after.refs, before.refs);
    assert_eq!(after.committed_version, before.committed_version);
    assert_eq!(
        codeyard_db_repos::repositories::records::get_repository_by_id(pool, f.repo)
            .await
            .unwrap()
            .unwrap()
            .default_branch,
        "renamed"
    );
    assert!(
        codeyard_db_repos::repositories::protection::branch_redirect(pool, f.repo, "renamed")
            .await
            .unwrap()
            .is_none()
    );
    stores.stop().await;
}

async fn namespace_rename_uses_both_stores(pool: &DbPool) {
    let stores = Stores::new(pool).await;
    let f = &stores.fixture;
    let store = stores.store(pool).await;
    for (old, new) in [("main", "main/topic"), ("main/topic", "main")] {
        store
            .rename_branch_as(pool, f.repo, f.owner, old, new)
            .await
            .unwrap();
        let refs = stores.refs().await;
        let reference = format!("refs/heads/{new}");
        assert_eq!(refs[0], refs[1]);
        assert_eq!(refs[0][&reference], stores.old);
        assert!(!refs[0].contains_key(&format!("refs/heads/{old}")));
        assert_eq!(stores.heads(), vec![reference; 2]);
        let write = last_write(pool, f.repo).await;
        assert!(write.finished);
        assert_eq!(write.votes.len(), 2);
    }
    stores.listeners[1].stop().await;
    set_timeout(pool, 1).await.unwrap();
    let refs = stores.refs().await;
    assert!(timeout(
        WAIT,
        store.rename_branch_as(pool, f.repo, f.owner, "main", "main/failed"),
    )
    .await
    .unwrap()
    .is_err());
    assert_eq!(stores.refs().await, refs);
    assert_eq!(stores.heads(), vec![MAIN; 2]);
    assert_eq!(last_write(pool, f.repo).await.decision, "abort");
    stores.stop().await;
}

async fn default_branch_uses_both_stores(pool: &DbPool) {
    let stores = Stores::new(pool).await;
    let f = &stores.fixture;
    let store = stores.store(pool).await;
    store
        .create_branch_as(pool, f.repo, f.owner, "other", &stores.old)
        .await
        .unwrap();
    store
        .set_default_branch_as(pool, f.repo, f.owner, "other")
        .await
        .unwrap();
    let other = "refs/heads/other";
    assert_eq!(stores.heads(), vec![other; 2]);
    let write = last_write(pool, f.repo).await;
    assert!(write.finished);
    assert_eq!(write.votes.len(), 2);
    assert_eq!(write.changes.len(), 1);
    assert_eq!(write.changes[0].name, "HEAD");
    assert_eq!(write.changes[0].old_target.as_deref(), Some(MAIN));
    assert_eq!(write.changes[0].new_target.as_deref(), Some(other));
    assert_eq!(
        codeyard_db_repos::repositories::records::get_repository_by_id(pool, f.repo)
            .await
            .unwrap()
            .unwrap()
            .default_branch,
        "other"
    );

    // A metadata refusal compensates HEAD through the same journal on both voters.
    let error = store
        .patch_repository_as(
            pool,
            f.repo,
            f.owner,
            "main",
            &codeyard_db_repos::repositories::records::RepositoryChanges {
                merge_methods: codeyard_db_repos::repositories::records::MergeMethodUpdate {
                    merge: Some(false),
                    squash: Some(false),
                    rebase: Some(false),
                },
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, codeyard_git::error::ErrorKind::Invalid);
    assert_eq!(stores.heads(), vec![other; 2]);
    let restored = codeyard_db_cluster::cluster::writes::recovery_view(pool, f.repo)
        .await
        .unwrap();
    assert_eq!(
        restored
            .refs
            .iter()
            .find(|value| value.name == "HEAD")
            .unwrap()
            .target,
        other
    );
    assert!(last_write(pool, f.repo).await.finished);

    stores.listeners[1].stop().await;
    set_timeout(pool, 1).await.unwrap();
    let refs = stores.refs().await;
    let error = timeout(
        WAIT,
        store.set_default_branch_as(pool, f.repo, f.owner, "main"),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.message, "write quorum unavailable");
    assert_eq!(stores.refs().await, refs);
    assert_eq!(stores.heads(), vec![other; 2]);
    let write = last_write(pool, f.repo).await;
    assert_eq!(write.decision, "abort");
    assert_eq!(write.votes.len(), 1);
    let after = codeyard_db_cluster::cluster::writes::recovery_view(pool, f.repo)
        .await
        .unwrap();
    assert_eq!(after.refs, restored.refs);
    assert_eq!(after.committed_version, restored.committed_version);
    assert_eq!(
        codeyard_db_repos::repositories::records::get_repository_by_id(pool, f.repo)
            .await
            .unwrap()
            .unwrap()
            .default_branch,
        "other"
    );
    stores.stop().await;
}

async fn new_repositories_use_all_voters(pool: &DbPool) {
    use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
    use codeyard_repos_core::repositories::lifecycle;
    let stores = Stores::with_voters(pool, 3).await;
    let f = &stores.fixture;
    let source = codeyard_db_repos::repositories::records::get_repository_by_id(pool, f.repo)
        .await
        .unwrap()
        .unwrap();
    for (name, source) in [("new-empty", None), ("new-copy", Some(&source))] {
        let repo = timeout(
            WAIT,
            lifecycle::provision(
                (pool, Some(&stores.contexts[0])),
                &stores.contexts[0].repos_path,
                RepositoryCreationOwner::personal(f.owner),
                name,
                None,
                false,
                source,
            ),
        )
        .await
        .expect("repository creation deadline")
        .unwrap();
        let view = codeyard_db_cluster::cluster::writes::recovery_view(pool, repo.id)
            .await
            .unwrap();
        assert!(view.serving);
        assert_eq!(view.voters.len(), 3);
        assert!(
            view.voters
                .iter()
                .filter(|voter| voter.applied_version == view.committed_version)
                .count()
                >= 2
        );
        let uuid = codeyard_repos_core::wire::repository_uuid(&repo.disk_path).unwrap();
        for context in &stores.contexts {
            let store = RepoStore::from_disk_path(
                term_path(&context.repos_path, uuid, 1).to_str().unwrap(),
            );
            assert_eq!(store.head_target().await.unwrap().as_deref(), Some(MAIN));
            if source.is_some() {
                codeyard_cluster::replicas::promote::catch_up_voter(
                    context,
                    repo.id,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap();
                assert_eq!(store.refs().await.unwrap().get(MAIN), Some(&stores.old));
            } else {
                assert!(store.refs().await.unwrap().is_empty());
            }
        }
        // Initialization cannot reset a serving term, including a replay from a voter.
        assert!(codeyard_repos_core::repositories::store::repo_store(
            &repo,
            Some(&stores.contexts[0])
        )
        .initialize(None, "other")
        .await
        .is_err());
        let reply = stores.contexts[1]
            .call(
                stores.contexts[0].node_id,
                codeyard_repos_core::wire::Request {
                    repository: uuid,
                    operation: codeyard_repos_core::wire::Operation::Initialize {
                        source: None,
                        default_branch: "other".into(),
                    },
                    actor: None,
                    ci_run: None,
                },
                b"",
            )
            .await
            .unwrap()
            .0;
        assert!(reply.result.is_err());
    }
    stores.listeners[1].stop().await;
    for (name, source) in [("failed-empty", None), ("failed-copy", Some(&source))] {
        assert!(timeout(
            WAIT,
            lifecycle::provision(
                (pool, Some(&stores.contexts[0])),
                &stores.contexts[0].repos_path,
                RepositoryCreationOwner::personal(f.owner),
                name,
                None,
                false,
                source,
            )
        )
        .await
        .expect("unavailable voter creation deadline")
        .is_err());
        assert!(codeyard_db_repos::repositories::records::get_repository(
            pool,
            "quorum-owner",
            name
        )
        .await
        .unwrap()
        .is_none());
    }
    assert_eq!(
        RepoStore::from_disk_path(stores.paths[0].to_str().unwrap())
            .refs()
            .await
            .unwrap()
            .get(MAIN),
        Some(&stores.old)
    );
    stores.stop().await;
}

macro_rules! engine_pair {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[test]
        fn $sqlite() {
            crate::support::large_stack::run(|| async {
                let database = native::NativeDatabase::new().await;
                $contract(&database.pool).await;
            });
        }
        #[test]
        fn $postgres() {
            crate::support::large_stack::run(|| async {
                let mut database = native::PostgresDatabase::new().await;
                $contract(&database.pool).await;
                database.finish().await;
            });
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
engine_pair!(
    sqlite_rename_is_one_write,
    postgres_rename_is_one_write,
    rename_is_one_write
);
engine_pair!(
    sqlite_namespace_rename_uses_both_stores,
    postgres_namespace_rename_uses_both_stores,
    namespace_rename_uses_both_stores
);
engine_pair!(
    sqlite_default_branch_uses_both_stores,
    postgres_default_branch_uses_both_stores,
    default_branch_uses_both_stores
);
engine_pair!(
    sqlite_new_repositories_use_all_voters,
    postgres_new_repositories_use_all_voters,
    new_repositories_use_all_voters
);

async fn acknowledged_push_is_advertised_when_the_primary_listener_disappears(pool: &DbPool) {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let stores = Stores::new(pool).await;
    let (result, _, _) = stores.push(pool).await;
    assert!(result.unwrap().success());
    let serving = &stores.contexts[1];
    match pool {
        DbPool::Postgres(raw) => {
            sqlx::query("UPDATE nodes SET roles=$2 WHERE id=$1")
                .bind(serving.node_id)
                .bind(vec!["web", "storage"])
                .execute(raw)
                .await
                .unwrap();
        }
        DbPool::Sqlite(raw) => {
            sqlx::query("UPDATE nodes SET roles=$2 WHERE id=$1")
                .bind(serving.node_id)
                .bind("[\"web\",\"storage\"]")
                .execute(raw)
                .await
                .unwrap();
        }
    }
    let router = codeyard_api_rest::create_router(codeyard_app::app::AppState::new(
        pool.clone(),
        serving.repos_path.clone(),
        Arc::new(
            codeyard_storage::ObjectStore::open(&serving.repos_path, None)
                .await
                .unwrap(),
        ),
        None,
        CiConfig::default(),
        Some(serving.clone()),
    ));
    stores.listeners[0].stop().await;
    let response = router
        .oneshot(
            Request::builder()
                .uri("/quorum-owner/quorum.git/info/refs?service=git-upload-pack")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains(&stores.new));
    stores.stop().await;
}
engine_pair!(
    sqlite_acknowledged_push_survives_primary_loss,
    postgres_acknowledged_push_survives_primary_loss,
    acknowledged_push_is_advertised_when_the_primary_listener_disappears
);

async fn three_voters_keep_a_majority_and_recover_a_missed_write(pool: &DbPool) {
    // The writes that must succeed keep the default 10-second write timeout: the fan-out
    // already returns at a majority, and a 2-second budget for receive, two fsynced
    // prepares and the vote poll was missed under a full parallel test run.
    let stores = Stores::with_voters(pool, 3).await;
    for voter in &stores.fixture.voters {
        let held = codeyard_db_cluster::cluster::writes::held_voters(pool, *voter)
            .await
            .unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].repository_id, stores.fixture.repo);
    }
    assert_eq!(
        codeyard_db_cluster::cluster::writes::write_family_terms(pool, stores.fixture.repo)
            .await
            .unwrap(),
        vec![(stores.fixture.repo, 1)]
    );
    // Stop one follower before a real receive-pack. Two durable voters still suffice.
    stores.listeners[2].stop().await;
    let (status, output, errors) = stores.push(pool).await;
    assert!(
        status.as_ref().is_ok_and(|status| status.success()),
        "{status:?}: {}",
        String::from_utf8_lossy(&errors)
    );
    assert!(
        String::from_utf8_lossy(&output).contains("ok refs/heads/main"),
        "{}{}",
        String::from_utf8_lossy(&output),
        String::from_utf8_lossy(&errors)
    );
    let write = last_write(pool, stores.fixture.repo).await;
    assert_eq!(write.required_votes, 2);
    assert!(write.finished);
    assert_eq!(stores.refs().await[1][MAIN], stores.new);
    assert_eq!(stores.refs().await[2][MAIN], stores.old);

    // The missed voter fetches acknowledged objects from a current copy. Its own
    // outbound connection works while its inbound listener is stopped.
    codeyard_cluster::replicas::pass(
        &stores.contexts[2],
        &mut codeyard_cluster::replicas::Retry::default(),
        &mut codeyard_cluster::replicas::promote::Silent::default(),
        &mut codeyard_cluster::replicas::Copying::default(),
    )
    .await
    .unwrap();
    assert_eq!(stores.refs().await[2][MAIN], stores.new);
    let view = codeyard_db_cluster::cluster::writes::recovery_view(pool, stores.fixture.repo)
        .await
        .unwrap();
    assert!(view
        .voters
        .iter()
        .all(|voter| voter.applied_version == view.committed_version));

    // Two stopped followers cannot acknowledge a branch mutation or change any refs.
    // Use the minimum timeout to cover optional remote cleanup consuming that budget.
    set_timeout(pool, 1).await.unwrap();
    stores.listeners[1].stop().await;
    let before = stores.refs().await;
    let store = stores.store(pool).await;
    assert!(store
        .create_branch_as(
            pool,
            stores.fixture.repo,
            stores.fixture.owner,
            "refused",
            &stores.new
        )
        .await
        .is_err());
    assert_eq!(stores.refs().await, before);
    let aborted = last_write(pool, stores.fixture.repo).await;
    assert_eq!(aborted.decision, "abort");
    assert!(
        aborted.finished,
        "offline cleanup must not block the next write"
    );
    stores.stop().await;
}
engine_pair!(
    sqlite_three_voters_keep_a_majority,
    postgres_three_voters_keep_a_majority,
    three_voters_keep_a_majority_and_recover_a_missed_write
);

async fn three_voter_recovery_refills_a_stale_survivor_while_public_reads_stay_fenced(
    pool: &DbPool,
) {
    let stores = Stores::with_voters(pool, 3).await;
    stores.listeners[2].stop().await;
    let (status, output, errors) = stores.push(pool).await;
    assert!(
        status.as_ref().is_ok_and(|status| status.success()),
        "{status:?}: {}",
        String::from_utf8_lossy(&errors)
    );
    assert!(String::from_utf8_lossy(&output).contains("ok refs/heads/main"));
    stores.listeners[0].stop().await;
    let repo = stores.fixture.repo;
    let target = stores.fixture.voters[1];
    let result =
        codeyard_cluster::replicas::promote::promote(pool, repo, Some(target), false).await;
    assert!(result
        .unwrap_err()
        .is::<codeyard_cluster::replicas::promote::RecoveryPending>());
    assert!(!codeyard_cluster::replicas::promote::recover_family(
        pool,
        repo,
        target,
        &stores.contexts[1].repos_path,
    )
    .await
    .unwrap());
    let row = codeyard_db_repos::repositories::records::get_repository_by_id(pool, repo)
        .await
        .unwrap()
        .unwrap();
    assert!(
        stores.contexts[1]
            .repository_store(&row, true)
            .await
            .is_err(),
        "fenced terms must refuse client reads"
    );
    // Even an active storage peer cannot obtain a fenced repository's objects
    // without being one of that term's assigned voters.
    let outsider_key = codeyard_support::node_identity::NodeKey::generate();
    let outsider = codeyard_db_cluster::cluster::register_own_node(
        pool,
        &outsider_key.public_hex(),
        "outsider",
        &["storage".into()],
        "test",
    )
    .await
    .unwrap();
    let mut foreign = stores.contexts[2].clone();
    foreign.node_id = outsider.id;
    foreign.key = outsider_key.clone();
    foreign.iroh = IrohDialer::new(Relay::Off, outsider_key);
    let mut leaked = Vec::new();
    let refused = timeout(
        WAIT,
        foreign.git_session(
            target,
            codeyard_repos_core::wire::Request {
                repository: codeyard_repos_core::wire::repository_uuid(&row.disk_path).unwrap(),
                operation: codeyard_repos_core::wire::Operation::ReplicaPack {
                    have: Vec::new(),
                    want: Vec::new(),
                },
                actor: None,
                ci_run: None,
            },
            GitStreams {
                input: &mut tokio::io::empty(),
                output: &mut leaked,
                errors: &mut tokio::io::sink(),
            },
        ),
    )
    .await
    .unwrap();
    assert!(refused.is_err());
    assert!(leaked.is_empty());
    foreign.iroh.close().await;

    let cancelled = tokio_util::sync::CancellationToken::new();
    cancelled.cancel();
    assert!(codeyard_cluster::replicas::promote::catch_up_voter(
        &stores.contexts[2],
        repo,
        &cancelled,
    )
    .await
    .is_err());
    let interrupted = codeyard_db_cluster::cluster::writes::recovery_view(pool, repo)
        .await
        .unwrap();
    assert_eq!(
        interrupted
            .voters
            .iter()
            .find(|voter| voter.node_id == stores.fixture.voters[2])
            .unwrap()
            .applied_version,
        -1
    );
    codeyard_cluster::replicas::promote::catch_up_voter(
        &stores.contexts[2],
        repo,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    // Already-current voters must still retry enabling a fenced family.
    codeyard_cluster::replicas::pass(
        &stores.contexts[2],
        &mut codeyard_cluster::replicas::Retry::default(),
        &mut codeyard_cluster::replicas::promote::Silent::default(),
        &mut codeyard_cluster::replicas::Copying::default(),
    )
    .await
    .unwrap();
    assert!(
        codeyard_db_cluster::cluster::writes::recovery_view(pool, repo)
            .await
            .unwrap()
            .serving
    );
    let current = stores.contexts[2]
        .repository_store(&row, true)
        .await
        .unwrap();
    assert_eq!(current.refs().await.unwrap()[MAIN], stores.new);
    assert!(
        stores.contexts[0]
            .repository_store(&row, true)
            .await
            .is_err(),
        "old primary cannot serve an unreconstructed term"
    );
    stores.stop().await;
}
engine_pair!(
    sqlite_three_voter_fenced_recovery,
    postgres_three_voter_fenced_recovery,
    three_voter_recovery_refills_a_stale_survivor_while_public_reads_stay_fenced
);

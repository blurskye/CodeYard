//! Two real iroh nodes, one shared PostgreSQL database, and separate disks.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client as http;
use crate::support::native_database as native;
use crate::support::rpc_operations;

use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::{list_workflows_for_ref, CiConfig},
    cluster::{
        self,
        rpc::{self, Failure, Operation, Reply, Request},
        NodeContext,
    },
    db,
    repositories::store::{
        repo_store, BlobRange, ErrorKind, FileWrite, GitService, GitSession, GitStreams,
        MergeMethod, MergeRequest, PreparedFile, RepoStore, MAX_ARCHIVE_RESPONSE_BYTES,
        MAX_BLOB_RESPONSE_BYTES, MAX_CI_WORKFLOW_BYTES, MAX_DIFF_FILE_PATCH_BYTES,
        MAX_EDITOR_BYTES, MAX_TREE_ENTRIES, MAX_WORKFLOW_DIRECTORY_ENTRIES,
    },
    storage::ObjectStore,
};
use rand::RngCore;
use serde::Serialize;
use serde_json::{json, Value};
use std::{path::Path, sync::Arc, time::Duration};
use uuid::Uuid;

/// The Git session cap is process-wide, as it is in production where one process is one node.
/// These tests share a process, so the one that fills the cap and the one that needs a session
/// permit take this lock and run one at a time.
static GIT_SESSION_CAP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Takes the session-cap lock after pinning the cap to 16 slots and an instant refusal, so the
/// tests that fill it do not depend on the CPU count or wait for a free slot.
async fn lock_git_session_cap() -> tokio::sync::MutexGuard<'static, ()> {
    static PIN: std::sync::Once = std::sync::Once::new();
    PIN.call_once(|| {
        std::env::set_var("GIT_SESSION_LIMIT", "16");
        std::env::set_var("GIT_SESSION_WAIT_SECONDS", "0");
    });
    GIT_SESSION_CAP.lock().await
}

/// A merge commit of `head` into `base`, committed by `name` and `email`.
fn merge_request(head: &str, base: &str, message: &str, name: &str, email: &str) -> MergeRequest {
    MergeRequest {
        head_branch: "feature".into(),
        base_branch: "main".into(),
        head: head.into(),
        base: base.into(),
        message: message.into(),
        name: name.into(),
        email: email.into(),
        method: MergeMethod::Merge,
        author_name: None,
        author_email: None,
    }
}

fn settings(name: &str) -> cluster::Settings {
    cluster::Settings::from_lookup(&|key| match key {
        "IROH_BIND" => Some("127.0.0.1:0".into()),
        "NODE_NAME" => Some(name.into()),
        _ => None,
    })
    .unwrap()
    .unwrap()
}

fn equal(left: impl Serialize, right: impl Serialize) {
    assert_eq!(
        serde_json::to_value(left).unwrap(),
        serde_json::to_value(right).unwrap()
    );
}

fn write(branch: &str, path: &str, revision: &str) -> FileWrite {
    FileWrite {
        branch: branch.into(),
        path: path.into(),
        message: format!("Add {path}"),
        expected_revision: revision.into(),
        creating: true,
        deleting: false,
    }
}

fn commit_workflow_entries(
    repository: &git2::Repository,
    reference: &str,
    entries: Vec<(String, Vec<u8>)>,
) -> git2::Oid {
    let mut workflows = repository.treebuilder(None).unwrap();
    for (name, content) in entries {
        let blob = repository.blob(&content).unwrap();
        workflows.insert(&name, blob, 0o100644).unwrap();
    }
    let workflows = workflows.write().unwrap();
    let mut github = repository.treebuilder(None).unwrap();
    github.insert("workflows", workflows, 0o040000).unwrap();
    let github = github.write().unwrap();
    let mut root = repository.treebuilder(None).unwrap();
    root.insert(".github", github, 0o040000).unwrap();
    let root = repository.find_tree(root.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repository
        .commit(
            Some(reference),
            &signature,
            &signature,
            "workflows",
            &root,
            &[],
        )
        .unwrap()
}

#[tokio::test]
async fn remote_first_reader_persists_languages() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let web = cluster::start(
        &database.pool,
        &root.path().join("web-state"),
        &root.path().join("web-repos"),
        &"web".parse().unwrap(),
        settings("language-web"),
    )
    .await
    .unwrap();
    web.publish().await.unwrap();
    let storage = cluster::start(
        &database.pool,
        &root.path().join("storage-state"),
        &root.path().join("storage-repos"),
        &"storage".parse().unwrap(),
        settings("language-storage"),
    )
    .await
    .unwrap();
    storage.publish().await.unwrap();
    let owner = db::users::accounts::create_user(&database.pool, "remote_language_owner", None)
        .await
        .unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (&database.pool, Some(&web.context)),
        &web.context.repos_path,
        db::repositories::creation::RepositoryCreationOwner::personal(owner.id),
        "languages",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let uuid = rpc::repository_uuid(&repository.disk_path).unwrap();
    let path = storage.context.repos_path.join(format!("{uuid}.git"));
    {
        let bare = git2::Repository::open_bare(&path).unwrap();
        let mut builder = bare.treebuilder(None).unwrap();
        builder
            .insert("lib.rs", bare.blob(b"fn main() {}\n").unwrap(), 0o100644)
            .unwrap();
        let tree = bare.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        bare.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Rust",
            &tree,
            &[],
        )
        .unwrap();
    }
    let remote = repo_store(&repository, Some(&web.context));
    let languages = codeyard::backend::repositories::languages::compute_and_store(
        &database.pool,
        &remote,
        &repository,
        "main",
    )
    .await
    .unwrap();
    assert_eq!(languages.len(), 1);
    assert_eq!(languages[0].language, "Rust");
    let rows = db::repository_stats::get_repo_languages(&database.pool, repository.id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].language, "Rust");
    storage.shutdown().await;
    web.shutdown().await;
    database.pool.close().await;
}

async fn compare_reads(remote: &RepoStore, local: &RepoStore, sha: &str) {
    equal(remote.exists().await, local.exists().await);
    equal(
        remote.size_bytes().await.unwrap(),
        local.size_bytes().await.unwrap(),
    );
    equal(remote.info().await.unwrap(), local.info().await.unwrap());
    equal(
        remote.commit_count().await.unwrap(),
        local.commit_count().await.unwrap(),
    );
    equal(remote.refs().await.unwrap(), local.refs().await.unwrap());
    equal(
        remote.tree("main", "").await.unwrap(),
        local.tree("main", "").await.unwrap(),
    );
    equal(
        remote.blob("main", "main.rs").await.unwrap(),
        local.blob("main", "main.rs").await.unwrap(),
    );
    equal(
        remote.blob_bytes("main", "main.rs").await.unwrap(),
        local.blob_bytes("main", "main.rs").await.unwrap(),
    );
    let blob_id = git2::Oid::hash_object(git2::ObjectType::Blob, b"fn main() {}\n")
        .unwrap()
        .to_string();
    equal(
        remote.blob_by_id(&blob_id).await.unwrap(),
        local.blob_by_id(&blob_id).await.unwrap(),
    );
    for (offset, limit) in [(0, 1), (1, 10)] {
        equal(
            remote
                .commits("main", offset, limit, &Default::default())
                .await
                .unwrap(),
            local
                .commits("main", offset, limit, &Default::default())
                .await
                .unwrap(),
        );
    }
    equal(
        remote.commit(sha).await.unwrap(),
        local.commit(sha).await.unwrap(),
    );
    equal(
        remote.commit_diff(sha).await.unwrap(),
        local.commit_diff(sha).await.unwrap(),
    );
    equal(
        remote.diff(sha, "main").await.unwrap(),
        local.diff(sha, "main").await.unwrap(),
    );
    equal(
        remote.changed_paths(sha, "main").await.unwrap(),
        local.changed_paths(sha, "main").await.unwrap(),
    );
    equal(remote.tags().await.unwrap(), local.tags().await.unwrap());
    equal(
        remote
            .archive(
                "main",
                "project",
                codeyard::backend::git::ArchiveFormat::Zip,
            )
            .await
            .unwrap(),
        local
            .archive(
                "main",
                "project",
                codeyard::backend::git::ArchiveFormat::Zip,
            )
            .await
            .unwrap(),
    );
    equal(
        remote.languages("main").await.unwrap(),
        local.languages("main").await.unwrap(),
    );
    equal(
        remote.contributors().await.unwrap(),
        local.contributors().await.unwrap(),
    );
    equal(
        remote.is_ancestor(sha, sha).await.unwrap(),
        local.is_ancestor(sha, sha).await.unwrap(),
    );
    equal(
        remote
            .resolve_ci_commit("main", Some(sha), true)
            .await
            .unwrap(),
        local
            .resolve_ci_commit("main", Some(sha), true)
            .await
            .unwrap(),
    );
    equal(
        remote
            .ci_workflow_file(sha, ".github/workflows/test.yml")
            .await
            .unwrap(),
        local
            .ci_workflow_file(sha, ".github/workflows/test.yml")
            .await
            .unwrap(),
    );
    equal(
        remote.advertise_refs(GitService::UploadPack).await.unwrap(),
        local.advertise_refs(GitService::UploadPack).await.unwrap(),
    );
    let want = format!("want {sha}\n");
    let request = format!("{:04x}{want}00000009done\n", want.len() + 4);
    let mut replies = Vec::new();
    for store in [remote, local] {
        let mut output = Vec::new();
        let status = store
            .git_session(
                GitSession::new(GitService::UploadPack, true),
                None,
                GitStreams {
                    input: &mut request.as_bytes(),
                    output: &mut output,
                    errors: &mut tokio::io::sink(),
                },
            )
            .await
            .unwrap();
        assert!(status.success());
        replies.push(output);
    }
    equal(&replies[0], &replies[1]);
}

async fn stream_call(connection: &iroh::endpoint::Connection, request: &Request) -> Reply {
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    rpc::write_frame(
        &mut send,
        &serde_json::to_vec(request).unwrap(),
        rpc::MAX_JSON,
    )
    .await
    .unwrap();
    rpc::write_frame(&mut send, b"", rpc::MAX_BYTES)
        .await
        .unwrap();
    send.finish().unwrap();
    let reply = rpc::read_frame(&mut recv, rpc::MAX_JSON).await.unwrap();
    assert!(rpc::read_frame(&mut recv, rpc::MAX_BYTES)
        .await
        .unwrap()
        .is_empty());
    serde_json::from_slice(&reply).unwrap()
}

async fn oversized(context: &NodeContext, storage: &cluster::Cluster, binary: bool, uuid: Uuid) {
    let connection = context
        .iroh
        .endpoint()
        .await
        .unwrap()
        .connect(storage.endpoint().unwrap().addr(), cluster::ALPN_RPC)
        .await
        .unwrap();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    if binary {
        rpc::write_frame(
            &mut send,
            &serde_json::to_vec(&Request {
                repository: uuid,
                operation: Operation::AdvertiseRefs {
                    service: GitService::UploadPack,
                },
                actor: None,
                ci_run: None,
            })
            .unwrap(),
            rpc::MAX_JSON,
        )
        .await
        .unwrap();
    }
    let limit = if binary {
        rpc::MAX_BYTES
    } else {
        rpc::MAX_JSON
    };
    send.write_all(&((limit + 1) as u32).to_be_bytes())
        .await
        .unwrap();
    send.finish().unwrap();
    assert!(tokio::time::timeout(
        Duration::from_secs(5),
        rpc::read_frame(&mut recv, rpc::MAX_JSON)
    )
    .await
    .unwrap()
    .is_err());
    connection.close(0u32.into(), b"test complete");
}

async fn rpc_reply(recv: &mut iroh::endpoint::RecvStream) -> Reply {
    let header = tokio::time::timeout(Duration::from_secs(3), rpc::read_frame(recv, rpc::MAX_JSON))
        .await
        .expect("repository RPC reply timed out")
        .unwrap();
    let reply = serde_json::from_slice(&header).unwrap();
    assert!(rpc::read_frame(recv, rpc::MAX_BYTES)
        .await
        .unwrap()
        .is_empty());
    reply
}

fn repository_error(reply: Reply) -> codeyard::backend::repositories::store::RepoError {
    match reply.result {
        Err(Failure::Repository(error)) => error,
        result => panic!("expected repository refusal, got {result:?}"),
    }
}

async fn body_refusal_without_payload(
    caller: &cluster::Cluster,
    storage: &cluster::Cluster,
    request: &Request,
) -> codeyard::backend::repositories::store::RepoError {
    let connection = caller
        .endpoint()
        .unwrap()
        .connect(storage.endpoint().unwrap().addr(), cluster::ALPN_RPC)
        .await
        .unwrap();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    rpc::write_frame(
        &mut send,
        &serde_json::to_vec(request).unwrap(),
        rpc::MAX_JSON,
    )
    .await
    .unwrap();
    send.write_all(&(rpc::MAX_BYTES as u32).to_be_bytes())
        .await
        .unwrap();
    let error = repository_error(rpc_reply(&mut recv).await);
    connection.close(0u32.into(), b"refusal received");
    error
}

async fn raw_editor_call(
    caller: &cluster::Cluster,
    storage: &cluster::Cluster,
    request: &Request,
    body: &[u8],
) -> Reply {
    let connection = caller
        .endpoint()
        .unwrap()
        .connect(storage.endpoint().unwrap().addr(), cluster::ALPN_RPC)
        .await
        .unwrap();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    rpc::write_frame(
        &mut send,
        &serde_json::to_vec(request).unwrap(),
        rpc::MAX_JSON,
    )
    .await
    .unwrap();
    rpc::write_frame(&mut send, body, MAX_EDITOR_BYTES)
        .await
        .unwrap();
    send.finish().unwrap();
    let reply = rpc_reply(&mut recv).await;
    connection.close(0u32.into(), b"editor call complete");
    reply
}

async fn open_git_session(
    caller: &cluster::Cluster,
    storage: &cluster::Cluster,
    repository: Uuid,
) -> (
    iroh::endpoint::Connection,
    iroh::endpoint::SendStream,
    iroh::endpoint::RecvStream,
    Reply,
) {
    let connection = caller
        .endpoint()
        .unwrap()
        .connect(storage.endpoint().unwrap().addr(), cluster::ALPN_RPC)
        .await
        .unwrap();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    rpc::write_frame(
        &mut send,
        &serde_json::to_vec(&Request {
            repository,
            operation: Operation::GitSession(GitSession::new(GitService::UploadPack, true)),
            actor: None,
            ci_run: None,
        })
        .unwrap(),
        rpc::MAX_JSON,
    )
    .await
    .unwrap();
    let header = tokio::time::timeout(
        Duration::from_secs(3),
        rpc::read_frame(&mut recv, rpc::MAX_JSON),
    )
    .await
    .expect("Git admission timed out")
    .unwrap();
    let reply = serde_json::from_slice(&header).unwrap();
    (connection, send, recv, reply)
}

#[tokio::test]
async fn repository_rpc_admits_bodies_and_git_sessions_with_separate_limits() {
    let _session_cap = lock_git_session_cap().await;
    let database = native::PostgresDatabase::new().await;
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let web = cluster::start(
        &database.pool,
        &root.path().join("web-state"),
        &root.path().join("web-repos"),
        &"web".parse().unwrap(),
        settings("admission-web"),
    )
    .await
    .unwrap();
    web.publish().await.unwrap();
    let storage = cluster::start(
        &database.pool,
        &root.path().join("storage-state"),
        &root.path().join("storage-repos"),
        &"storage".parse().unwrap(),
        settings("admission-storage"),
    )
    .await
    .unwrap();
    storage.publish().await.unwrap();
    let worker = cluster::start(
        &database.pool,
        &root.path().join("worker-state"),
        &root.path().join("worker-repos"),
        &"actions-worker".parse().unwrap(),
        settings("admission-worker"),
    )
    .await
    .unwrap();
    worker.publish().await.unwrap();
    let storage_caller = cluster::start(
        &database.pool,
        &root.path().join("caller-state"),
        &root.path().join("caller-repos"),
        &"storage".parse().unwrap(),
        settings("admission-storage-caller"),
    )
    .await
    .unwrap();
    storage_caller.publish().await.unwrap();
    let owner = db::users::accounts::create_user(&database.pool, "admission_owner", None)
        .await
        .unwrap();
    let writer = db::users::accounts::create_user(&database.pool, "admission_writer", None)
        .await
        .unwrap();
    let stranger = db::users::accounts::create_user(&database.pool, "admission_stranger", None)
        .await
        .unwrap();
    let uuid = Uuid::new_v4();
    let disk_path = root.path().join(format!("{uuid}.git"));
    let repository = db::repositories::records::create_repository(
        &database.pool,
        owner.id,
        "admission-project",
        None,
        false,
        disk_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let pg = &database.raw;
    sqlx::query("UPDATE repositories SET storage_node_id=$1 WHERE id=$2")
        .bind(storage.context.node_id)
        .bind(repository.id)
        .execute(pg)
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(
        &database.pool,
        writer.id,
        repository.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let local_path = storage.context.repos_path.join(format!("{uuid}.git"));
    let local = RepoStore::from_disk_path(local_path.to_str().unwrap());
    local.initialize(None, "main").await.unwrap();

    let save = |path: &str, revision: &str| Operation::SaveFile {
        write: write("main", path, revision),
        author: "RPC author".into(),
        email: "rpc@example.invalid".into(),
    };
    let prepare = |path: &str, revision: &str| Operation::PrepareFile {
        write: write("main", path, revision),
        author: "RPC author".into(),
        email: "rpc@example.invalid".into(),
    };

    for (caller, request, message) in [
        (
            &worker,
            Request {
                repository: uuid,
                operation: prepare("worker.txt", "unborn"),
                actor: None,
                ci_run: None,
            },
            "requires a web or ssh node",
        ),
        (
            &storage_caller,
            Request {
                repository: uuid,
                operation: prepare("storage.txt", "unborn"),
                actor: None,
                ci_run: None,
            },
            "requires a web or ssh node",
        ),
        (
            &web,
            Request {
                repository: uuid,
                operation: save("missing-actor.txt", "unborn"),
                actor: None,
                ci_run: None,
            },
            "requires an actor",
        ),
        (
            &web,
            Request {
                repository: uuid,
                operation: save("wrong-actor.txt", "unborn"),
                actor: Some(stranger.id),
                ci_run: None,
            },
            "repository write denied",
        ),
    ] {
        let error = body_refusal_without_payload(caller, &storage, &request).await;
        assert_eq!(error.kind, ErrorKind::Invalid);
        assert!(error.message.contains(message), "{error:?}");
    }

    let connection = web
        .endpoint()
        .unwrap()
        .connect(storage.endpoint().unwrap().addr(), cluster::ALPN_RPC)
        .await
        .unwrap();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    rpc::write_frame(
        &mut send,
        &serde_json::to_vec(&Request {
            repository: uuid,
            operation: Operation::Refs,
            actor: None,
            ci_run: None,
        })
        .unwrap(),
        rpc::MAX_JSON,
    )
    .await
    .unwrap();
    send.write_all(&1u32.to_be_bytes()).await.unwrap();
    assert!(tokio::time::timeout(
        Duration::from_secs(3),
        rpc::read_frame(&mut recv, rpc::MAX_JSON)
    )
    .await
    .expect("bodyless frame refusal timed out")
    .is_err());
    connection.close(0u32.into(), b"bodyless frame refused");

    let content = vec![b'x'; MAX_EDITOR_BYTES];
    let prepared: PreparedFile = serde_json::from_value(
        raw_editor_call(
            &web,
            &storage,
            &Request {
                repository: uuid,
                operation: prepare("prepared.txt", "unborn"),
                actor: None,
                ci_run: None,
            },
            &content,
        )
        .await
        .result
        .unwrap(),
    )
    .unwrap();
    assert_eq!(prepared.expected, None);
    let saved: codeyard::backend::repositories::store::CommitInfo = serde_json::from_value(
        raw_editor_call(
            &web,
            &storage,
            &Request {
                repository: uuid,
                operation: save("saved.txt", "unborn"),
                actor: Some(owner.id),
                ci_run: None,
            },
            &content,
        )
        .await
        .result
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        local
            .blob_bytes("main", "saved.txt")
            .await
            .unwrap()
            .bytes
            .len(),
        MAX_EDITOR_BYTES
    );

    let connection = web
        .endpoint()
        .unwrap()
        .connect(storage.endpoint().unwrap().addr(), cluster::ALPN_RPC)
        .await
        .unwrap();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    rpc::write_frame(
        &mut send,
        &serde_json::to_vec(&Request {
            repository: uuid,
            operation: prepare("too-large.txt", &saved.sha),
            actor: None,
            ci_run: None,
        })
        .unwrap(),
        rpc::MAX_JSON,
    )
    .await
    .unwrap();
    send.write_all(&((MAX_EDITOR_BYTES + 1) as u32).to_be_bytes())
        .await
        .unwrap();
    assert!(tokio::time::timeout(
        Duration::from_secs(3),
        rpc::read_frame(&mut recv, rpc::MAX_JSON)
    )
    .await
    .expect("oversized editor frame refusal timed out")
    .is_err());
    connection.close(0u32.into(), b"oversized editor frame refused");

    let mut lock = pg.begin().await.unwrap();
    sqlx::query("LOCK TABLE branch_protection_rules IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let connection = web
        .endpoint()
        .unwrap()
        .connect(storage.endpoint().unwrap().addr(), cluster::ALPN_RPC)
        .await
        .unwrap();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    let request = Request {
        repository: uuid,
        operation: save("revoked-during-upload.txt", &saved.sha),
        actor: Some(writer.id),
        ci_run: None,
    };
    rpc::write_frame(
        &mut send,
        &serde_json::to_vec(&request).unwrap(),
        rpc::MAX_JSON,
    )
    .await
    .unwrap();
    send.write_all(&7u32.to_be_bytes()).await.unwrap();
    send.write_all(b"revoke").await.unwrap();
    let blocked_pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(pid) = sqlx::query_scalar::<_, i32>(
                "SELECT pid FROM pg_stat_activity \
                 WHERE application_name=current_setting('application_name') \
                   AND wait_event_type='Lock' AND query ILIKE '%branch_protection_rules%' \
                 LIMIT 1",
            )
            .fetch_optional(pg)
            .await
            .unwrap()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("early editor authorization did not reach branch protection");
    lock.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state =
                sqlx::query_scalar::<_, String>("SELECT state FROM pg_stat_activity WHERE pid=$1")
                    .bind(blocked_pid)
                    .fetch_optional(pg)
                    .await
                    .unwrap();
            if state.as_deref() != Some("active") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("early editor authorization did not finish");
    assert!(
        db::repositories::access::revoke_repo_access(&database.pool, writer.id, repository.id,)
            .await
            .unwrap()
    );
    send.write_all(b"d").await.unwrap();
    send.finish().unwrap();
    let error = repository_error(rpc_reply(&mut recv).await);
    assert!(error.is_write_denied(), "{error:?}");
    connection.close(0u32.into(), b"revoked upload complete");
    assert_eq!(local.commit("main").await.unwrap().sha, saved.sha);
    assert_eq!(
        local
            .blob("main", "revoked-during-upload.txt")
            .await
            .unwrap_err()
            .kind,
        ErrorKind::MissingPath
    );

    let mut sessions = Vec::new();
    for _ in 0..16 {
        let (connection, send, recv, reply) = open_git_session(&web, &storage, uuid).await;
        assert!(reply.result.is_ok(), "Git session admission failed");
        sessions.push((connection, send, recv));
    }
    let refs = tokio::time::timeout(Duration::from_secs(3), async {
        let connection = web
            .endpoint()
            .unwrap()
            .connect(storage.endpoint().unwrap().addr(), cluster::ALPN_RPC)
            .await
            .unwrap();
        let reply = stream_call(
            &connection,
            &Request {
                repository: uuid,
                operation: Operation::Refs,
                actor: None,
                ci_run: None,
            },
        )
        .await;
        connection.close(0u32.into(), b"refs complete");
        reply
    })
    .await
    .expect("Refs waited behind active Git sessions");
    assert!(refs.result.is_ok());
    let (busy_connection, _busy_send, mut busy_recv, busy) =
        open_git_session(&web, &storage, uuid).await;
    let error = repository_error(busy);
    assert_eq!(error.message, "Git sessions busy");
    assert!(rpc::read_frame(&mut busy_recv, rpc::MAX_BYTES)
        .await
        .unwrap()
        .is_empty());
    busy_connection.close(0u32.into(), b"busy received");

    for (connection, _send, _recv) in sessions {
        connection.close(0u32.into(), b"test closes held Git session");
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (connection, _send, _recv, reply) = open_git_session(&web, &storage, uuid).await;
            connection.close(0u32.into(), b"cleanup checked");
            if reply.result.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("Git session permits or processes did not clean up");

    storage_caller.shutdown().await;
    worker.shutdown().await;
    web.shutdown().await;
    storage.shutdown().await;
}

#[tokio::test]
async fn control_heavy_blame_uses_the_raw_response_body() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let web = cluster::start(
        &database.pool,
        &root.path().join("web-state"),
        &root.path().join("web"),
        &"web".parse().unwrap(),
        settings("blame-web"),
    )
    .await
    .unwrap();
    let storage = cluster::start(
        &database.pool,
        &root.path().join("storage-state"),
        &root.path().join("storage"),
        &"storage".parse().unwrap(),
        settings("blame-storage"),
    )
    .await
    .unwrap();
    web.publish().await.unwrap();
    storage.publish().await.unwrap();
    let local = RepoStore::create(&storage.context.repos_path, "main", None)
        .await
        .unwrap();
    {
        let repo = git2::Repository::open_bare(local.location()).unwrap();
        let signature = git2::Signature::now("Tester", "test@example.invalid").unwrap();
        let mut parent = None;
        for version in 0..2 {
            let mut content = Vec::new();
            for line in 0..250 {
                let mut bytes = vec![if line < 3 { b'a' } else { 1 }; 3998];
                if version == 1 && line % 2 == 0 {
                    bytes[0] = b'x';
                }
                content.extend(bytes);
                content.push(b'\n');
            }
            let mut builder = repo.treebuilder(None).unwrap();
            builder
                .insert("control.txt", repo.blob(&content).unwrap(), 0o100644)
                .unwrap();
            let tree = repo.find_tree(builder.write().unwrap()).unwrap();
            let previous = parent.map(|id| repo.find_commit(id).unwrap());
            parent = Some(
                repo.commit(
                    Some("refs/heads/main"),
                    &signature,
                    &signature,
                    &char::from(1 + version).to_string().repeat(4000),
                    &tree,
                    &previous.iter().collect::<Vec<_>>(),
                )
                .unwrap(),
            );
        }
    }
    let owner = db::users::accounts::create_user(&database.pool, "blame_owner", None)
        .await
        .unwrap();
    let repo = db::repositories::records::create_repository(
        &database.pool,
        owner.id,
        "controls",
        None,
        false,
        local.location(),
    )
    .await
    .unwrap();
    db::cluster::place_repository(&database.pool, repo.id, storage.context.node_id)
        .await
        .unwrap();
    let expected = local.blame("main", "control.txt").await.unwrap();
    assert!(serde_json::to_vec(&expected).unwrap().len() > rpc::MAX_JSON);
    assert_eq!(
        repo_store(&repo, Some(&web.context))
            .blame("main", "control.txt")
            .await
            .unwrap(),
        expected
    );
    web.shutdown().await;
    storage.shutdown().await;
}

#[tokio::test]
async fn remote_workflow_discovery_uses_the_bounded_rpc() {
    let database = native::PostgresDatabase::new().await;
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let web = cluster::start(
        &database.pool,
        &root.path().join("workflow-web-state"),
        &root.path().join("workflow-web-repos"),
        &"web".parse().unwrap(),
        settings("workflow-rpc-web"),
    )
    .await
    .unwrap();
    web.publish().await.unwrap();
    let storage = cluster::start(
        &database.pool,
        &root.path().join("workflow-storage-state"),
        &root.path().join("workflow-storage-repos"),
        &"storage".parse().unwrap(),
        settings("workflow-rpc-storage"),
    )
    .await
    .unwrap();
    storage.publish().await.unwrap();
    let owner = db::users::accounts::create_user(&database.pool, "workflow_rpc_owner", None)
        .await
        .unwrap();
    let uuid = Uuid::new_v4();
    let repository = db::repositories::records::create_repository(
        &database.pool,
        owner.id,
        "workflow-rpc",
        None,
        false,
        root.path().join(format!("{uuid}.git")).to_str().unwrap(),
    )
    .await
    .unwrap();
    let pg = &database.raw;
    sqlx::query("UPDATE repositories SET storage_node_id=$1 WHERE id=$2")
        .bind(storage.context.node_id)
        .bind(repository.id)
        .execute(pg)
        .await
        .unwrap();
    let repository = db::repositories::records::get_repository_by_id(&database.pool, repository.id)
        .await
        .unwrap()
        .unwrap();

    let storage_path = storage.context.repos_path.join(format!("{uuid}.git"));
    let bare = git2::Repository::init_bare(&storage_path).unwrap();
    let mut exact = b"name: Limit\non: push\n".to_vec();
    exact.resize(MAX_CI_WORKFLOW_BYTES, b' ');
    commit_workflow_entries(
        &bare,
        "refs/heads/main",
        vec![
            ("z.yml".into(), b"name: Alpha\non: push\n".to_vec()),
            ("a.yaml".into(), b"name: Zebra\non: push\n".to_vec()),
            ("limit.yml".into(), exact),
            ("broken.yml".into(), b"on: [push\n".to_vec()),
            (
                "oversized.yml".into(),
                vec![b'#'; MAX_CI_WORKFLOW_BYTES + 1],
            ),
            (
                format!("{}.yml", "n".repeat(252)),
                b"name: too long a file name\non: push\n".to_vec(),
            ),
        ],
    );
    commit_workflow_entries(
        &bare,
        "refs/heads/crowded",
        (0..=MAX_WORKFLOW_DIRECTORY_ENTRIES)
            .map(|index| (format!("entry-{index}.txt"), Vec::new()))
            .collect(),
    );
    drop(bare);

    let local = repo_store(&repository, Some(&storage.context));
    let remote = repo_store(&repository, Some(&web.context));
    equal(
        remote.workflow_files("main").await.unwrap(),
        local.workflow_files("main").await.unwrap(),
    );
    let template_paths = vec![
        ".github/workflows/a.yaml".into(),
        ".github/workflows".into(),
        "missing.md".into(),
        ".github/workflows/oversized.yml".into(),
    ];
    let template_texts = remote
        .template_texts("main", &template_paths)
        .await
        .unwrap();
    equal(
        &template_texts,
        local.template_texts("main", &template_paths).await.unwrap(),
    );
    assert_eq!(
        template_texts[0].as_ref().unwrap().1,
        "name: Zebra\non: push\n"
    );
    assert!(template_texts[1..].iter().all(Option::is_none));
    assert!(remote
        .template_texts("main", &vec!["missing.md".into(); 23])
        .await
        .is_err());
    let workflows = list_workflows_for_ref(&remote, "main").await.unwrap();
    assert_eq!(
        workflows
            .iter()
            .map(|workflow| workflow.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Alpha", "Limit", "Zebra"]
    );
    let local_error = local.workflow_files("crowded").await.unwrap_err();
    let remote_error = remote.workflow_files("crowded").await.unwrap_err();
    equal(&remote_error, &local_error);
    assert!(remote_error.message.contains("entry limit"));

    web.shutdown().await;
    storage.shutdown().await;
}

#[test]
fn repository_rpc_operation_role_table() {
    for (operation, worker_read) in rpc_operations::operations(&"1".repeat(40)) {
        for (roles, allowed) in [
            (vec!["web"], true),
            (vec!["ssh"], true),
            (vec!["actions-worker"], worker_read),
            (vec!["storage"], false),
            (vec!["web", "actions-worker"], true),
            (vec!["ssh", "storage"], true),
            (vec!["storage", "actions-worker"], worker_read),
            (vec![], false),
        ] {
            let roles: Vec<String> = roles.into_iter().map(str::to_owned).collect();
            // A replica copy's pack request needs the storage role; the other
            // operations need web or ssh, or a worker's read.
            let replica = matches!(operation, Operation::ReplicaPack { .. });
            let allowed = if replica {
                roles.iter().any(|role| role == "storage")
            } else {
                allowed
            };
            let result = rpc::authorize_operation(&roles, &operation);
            assert_eq!(result.is_ok(), allowed, "{roles:?}: {operation:?}");
            if let Err(error) = result {
                assert_eq!(error.kind, ErrorKind::Invalid);
                let refusal = if replica {
                    "requires a storage node"
                } else {
                    "requires a web or ssh node"
                };
                assert!(error.message.contains(refusal));
            }
        }
    }
}

async fn unread_repository_downloads_are_bounded(router: &axum::Router) {
    use axum::{
        body::{to_bytes, Body},
        http::{header, Request},
    };
    use tower::ServiceExt;

    let paths = [
        "/api/repos/rpc_owner/project/raw/download-budget/download.bin",
        "/api/repos/rpc_owner/project/archive?ref=download-budget",
        "/rpc_owner/project/raw/download-budget/download.bin",
        "/rpc_owner/project/archive/download-budget.zip",
    ];
    let request = |path: &str| {
        Request::builder()
            .uri(path)
            .header(header::ACCEPT_ENCODING, "identity")
            .body(Body::empty())
            .unwrap()
    };
    let mut responses = Vec::new();
    for path in paths {
        let response = router.clone().oneshot(request(path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        responses.push(response);
    }
    // A missing object/ref must not even be materialized while the budget is held.
    for path in [
        "/api/repos/rpc_owner/project/raw/download-budget/missing.bin",
        "/api/repos/rpc_owner/project/archive?ref=missing",
        "/rpc_owner/project/raw/download-budget/missing.bin",
        "/rpc_owner/project/archive/missing.zip",
    ] {
        let response = router.clone().oneshot(request(path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
    }
    drop(responses.pop());
    tokio::task::yield_now().await;
    let response = router.clone().oneshot(request(paths[0])).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(bytes.len(), 256 * 1024);

    // Unpolled responses remain alive, but their producers expire under backpressure.
    tokio::time::sleep(Duration::from_secs(31)).await;
    let response = router.clone().oneshot(request(paths[1])).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .starts_with(b"PK"));
    for response in responses {
        assert!(to_bytes(response.into_body(), 1024 * 1024).await.is_err());
    }
}

#[tokio::test]
async fn web_node_uses_remote_storage_and_rpc_checks_every_request() {
    let _session_cap = lock_git_session_cap().await;
    bundle::install();
    let database = native::PostgresDatabase::new().await; // Missing PostgreSQL must fail.
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let web_disk = tempfile::tempdir().unwrap();
    let storage_disk = tempfile::tempdir().unwrap();
    let web = cluster::start(
        &database.pool,
        &web_disk.path().join("state"),
        web_disk.path(),
        &"web,ssh".parse().unwrap(),
        settings("rpc-web"),
    )
    .await
    .unwrap();
    web.publish().await.unwrap();
    let storage = cluster::start(
        &database.pool,
        &storage_disk.path().join("state"),
        storage_disk.path(),
        &"storage".parse().unwrap(),
        settings("rpc-storage"),
    )
    .await
    .unwrap();
    storage.publish().await.unwrap();
    let stale = tokio::time::timeout(
        Duration::from_secs(5),
        web.endpoint()
            .unwrap()
            .connect(storage.endpoint().unwrap().addr(), b"codeyard/rpc/1"),
    )
    .await;
    assert!(
        !matches!(stale, Ok(Ok(_))),
        "a stale RPC generation reached the server"
    );

    let reported = db::cluster::get_node_by_name(&database.pool, "rpc-storage")
        .await
        .unwrap()
        .unwrap();
    assert!(reported.disk_free.unwrap() > 0);
    let owner = db::users::accounts::create_user(&database.pool, "rpc_owner", None)
        .await
        .unwrap();
    let fork_owner = db::users::accounts::create_user(&database.pool, "rpc_forker", None)
        .await
        .unwrap();
    let router = create_router(AppState::new(
        database.pool.clone(),
        web_disk.path().to_owned(),
        Arc::new(ObjectStore::open(web_disk.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        Some(web.context.clone()),
    ));
    let client = http::Client::login(&router, &database.pool, owner.id).await;
    let created = client
        .expect(
            "POST",
            "/api/repos",
            json!({"name":"project","is_private":false}),
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
    assert_eq!(repository.storage_node_id, Some(storage.context.node_id));
    // A storage node placing its own repository records itself: other nodes route by it.
    assert_eq!(
        cluster::placement::storage_node(Some(&storage.context), None)
            .await
            .unwrap(),
        Some(storage.context.node_id)
    );
    let uuid = rpc::repository_uuid(&repository.disk_path).unwrap();
    let storage_path = storage_disk.path().join(format!("{uuid}.git"));
    assert!(!Path::new(&repository.disk_path).exists());
    assert!(storage_path.exists());
    let remote = repo_store(&repository, Some(&web.context));
    let local = repo_store(&repository, Some(&storage.context));
    equal(
        remote
            .advertise_refs(GitService::ReceivePack)
            .await
            .unwrap(),
        local.advertise_refs(GitService::ReceivePack).await.unwrap(),
    );

    let initial = remote
        .save_file_as(
            &database.pool,
            repository.id,
            owner.id,
            &write("main", "main.rs", "unborn"),
            b"fn main() {}\n",
            "Tester",
            "tester@example.invalid",
        )
        .await
        .unwrap();
    equal(&initial, local.commit(&initial.sha).await.unwrap());
    // Simulate an adopted repository with no language snapshot. Its first web-node
    // reader must populate the shared rows on the storage node, including Explore.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if !db::repository_stats::get_repo_languages(&database.pool, repository.id)
                .await
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    db::repository_stats::upsert_repo_languages(
        &database.pool,
        &db::repository_stats::language_source(&database.pool, repository.id)
            .await
            .unwrap()
            .unwrap(),
        "obsolete-revision",
        &[],
    )
    .await
    .unwrap();
    let languages = client
        .expect(
            "GET",
            "/api/repos/rpc_owner/project/languages",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(languages[0]["language"], "Rust");
    let stored = db::repository_stats::get_repo_languages(&database.pool, repository.id)
        .await
        .unwrap();
    assert_eq!(stored[0].language, "Rust");
    let (status, explore) = client.page("/explore?language=Rust").await;
    assert_eq!(status, StatusCode::OK);
    assert!(explore.contains("/rpc_owner/project"));
    // Adopted repositories may have a Git default different from the stored default.
    {
        let bare = git2::Repository::open_bare(&storage_path).unwrap();
        bare.reference(
            "refs/heads/trunk",
            git2::Oid::from_str(&initial.sha).unwrap(),
            false,
            "",
        )
        .unwrap();
        bare.set_head("refs/heads/trunk").unwrap();
    }
    db::repository_stats::upsert_repo_languages(
        &database.pool,
        &db::repository_stats::language_source(&database.pool, repository.id)
            .await
            .unwrap()
            .unwrap(),
        "obsolete-revision",
        &[],
    )
    .await
    .unwrap();
    client
        .expect(
            "GET",
            "/api/repos/rpc_owner/project/languages",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        db::repository_stats::get_repo_languages(&database.pool, repository.id)
            .await
            .unwrap()[0]
            .language,
        "Rust"
    );
    {
        let bare = git2::Repository::open_bare(&storage_path).unwrap();
        bare.set_head("refs/heads/main").unwrap();
        bare.find_reference("refs/heads/trunk")
            .unwrap()
            .delete()
            .unwrap();
    }
    let binary = client.expect(
        "POST", "/api/repos/rpc_owner/project/contents/image.bin",
        json!({"content":"AP8=", "message":"Add binary", "branch":"main", "expected_revision":initial.sha}),
        StatusCode::CREATED,
    ).await;
    let binary_sha = binary["sha"].as_str().unwrap();
    equal(
        remote.blob_bytes("main", "image.bin").await.unwrap(),
        local.blob_bytes("main", "image.bin").await.unwrap(),
    );
    assert_eq!(
        remote.blob_bytes("main", "image.bin").await.unwrap().bytes,
        [0, 255]
    );
    let workflow = remote.save_file_as(&database.pool, repository.id, owner.id, &write("main", ".github/workflows/test.yml", binary_sha), b"name: test\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo test\n", "Tester", "tester@example.invalid").await.unwrap();

    // Local and remote reads inspect the object/output bound before collecting it.
    let mut oversized_blob = vec![0u8; MAX_BLOB_RESPONSE_BYTES + 1];
    rand::rngs::OsRng.fill_bytes(&mut oversized_blob);
    let range_start = MAX_BLOB_RESPONSE_BYTES as u64 - 15;
    let expected_range = oversized_blob[range_start as usize..range_start as usize + 16].to_vec();
    let (wide_tree, large_diff, large_commit) = {
        let bare = git2::Repository::open_bare(&storage_path).unwrap();
        let parent = bare
            .find_commit(git2::Oid::from_str(&workflow.sha).unwrap())
            .unwrap();
        let parent_tree = parent.tree().unwrap();
        let blob = bare.blob(&oversized_blob).unwrap();
        let mut tree = bare.treebuilder(Some(&parent_tree)).unwrap();
        tree.insert("oversized.bin", blob, 0o100644).unwrap();
        let tree = bare.find_tree(tree.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        bare.commit(
            Some("refs/heads/oversized"),
            &signature,
            &signature,
            "oversized response fixture",
            &tree,
            &[&parent],
        )
        .unwrap();

        let mut download_tree = bare.treebuilder(None).unwrap();
        download_tree
            .insert(
                "download.bin",
                bare.blob(&oversized_blob[..256 * 1024]).unwrap(),
                0o100644,
            )
            .unwrap();
        let download_tree = bare.find_tree(download_tree.write().unwrap()).unwrap();
        bare.commit(
            Some("refs/heads/download-budget"),
            &signature,
            &signature,
            "download budget",
            &download_tree,
            &[],
        )
        .unwrap();

        let entry = bare.blob(b"entry\n").unwrap();
        let mut tree = bare.treebuilder(None).unwrap();
        for index in 0..=MAX_TREE_ENTRIES {
            tree.insert(format!("file-{index:04}.txt"), entry, 0o100644)
                .unwrap();
        }
        let tree = bare.find_tree(tree.write().unwrap()).unwrap();
        let wide_tree = bare
            .commit(
                Some("refs/heads/wide-tree"),
                &signature,
                &signature,
                "wide tree",
                &tree,
                &[],
            )
            .unwrap();

        let content = b"changed line\n".repeat(MAX_DIFF_FILE_PATCH_BYTES / 4);
        let mut tree = bare.treebuilder(Some(&parent_tree)).unwrap();
        tree.insert("large-diff.txt", bare.blob(&content).unwrap(), 0o100644)
            .unwrap();
        let tree = bare.find_tree(tree.write().unwrap()).unwrap();
        let large_diff = bare
            .commit(
                Some("refs/heads/large-diff"),
                &signature,
                &signature,
                "large diff",
                &tree,
                &[&parent],
            )
            .unwrap();
        let large_commit = bare
            .commit(
                Some("refs/heads/large-commit"),
                &signature,
                &signature,
                &"x".repeat(codeyard::backend::git::MAX_COMMIT_OBJECT_BYTES),
                &parent_tree,
                &[&parent],
            )
            .unwrap();
        (wide_tree, large_diff, large_commit)
    };
    drop(oversized_blob);
    let local_router = create_router(AppState::new(
        database.pool.clone(),
        storage_disk.path().to_owned(),
        Arc::new(ObjectStore::open(storage_disk.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        Some(storage.context.clone()),
    ));
    tokio::join!(
        unread_repository_downloads_are_bounded(&router),
        unread_repository_downloads_are_bounded(&local_router),
    );
    let local_error = local
        .blob_bytes("oversized", "oversized.bin")
        .await
        .unwrap_err();
    let remote_error = remote
        .blob_bytes("oversized", "oversized.bin")
        .await
        .unwrap_err();
    assert!(local_error.is_response_too_large());
    equal(&remote_error, &local_error);
    for store in [&local, &remote] {
        let range = store
            .blob_range(
                "oversized",
                "oversized.bin",
                BlobRange::Bytes {
                    start: range_start,
                    end: Some(range_start + 15),
                },
            )
            .await
            .unwrap();
        assert_eq!(range.bytes, expected_range);
        assert_eq!(range.size, MAX_BLOB_RESPONSE_BYTES as u64 + 1);
        assert_eq!(range.offset, range_start);
    }
    let invalid = BlobRange::Bytes {
        start: MAX_BLOB_RESPONSE_BYTES as u64 + 1,
        end: None,
    };
    let local_error = local
        .blob_range("oversized", "oversized.bin", invalid)
        .await
        .unwrap_err();
    let remote_error = remote
        .blob_range("oversized", "oversized.bin", invalid)
        .await
        .unwrap_err();
    assert!(local_error.is_range_not_satisfiable());
    equal(&remote_error, &local_error);
    let local_error = local
        .archive(
            "oversized",
            "project",
            codeyard::backend::git::ArchiveFormat::Zip,
        )
        .await
        .unwrap_err();
    let remote_error = remote
        .archive(
            "oversized",
            "project",
            codeyard::backend::git::ArchiveFormat::Zip,
        )
        .await
        .unwrap_err();
    assert!(local_error.is_response_too_large());
    equal(&remote_error, &local_error);
    assert_eq!(MAX_ARCHIVE_RESPONSE_BYTES, MAX_BLOB_RESPONSE_BYTES);

    let local_error = local.tree(&wide_tree.to_string(), "").await.unwrap_err();
    let remote_error = remote.tree(&wide_tree.to_string(), "").await.unwrap_err();
    assert!(local_error.is_response_too_large());
    equal(&remote_error, &local_error);
    let local_error = local
        .commit_diff(&large_diff.to_string())
        .await
        .unwrap_err();
    let remote_error = remote
        .commit_diff(&large_diff.to_string())
        .await
        .unwrap_err();
    assert!(local_error.is_response_too_large());
    equal(&remote_error, &local_error);
    let local_error = local
        .commits(&large_commit.to_string(), 0, 1, &Default::default())
        .await
        .unwrap_err();
    let remote_error = remote
        .commits(&large_commit.to_string(), 0, 1, &Default::default())
        .await
        .unwrap_err();
    assert!(local_error.is_response_too_large());
    equal(&remote_error, &local_error);

    let late_writer = db::users::accounts::create_user(&database.pool, "rpc_late_writer", None)
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(
        &database.pool,
        late_writer.id,
        repository.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let denied = remote
        .prepare_file(
            &write("main", "denied-after-prepare.txt", &workflow.sha),
            b"must stay unreachable\n",
            "Late writer",
            "late@example.invalid",
        )
        .await
        .unwrap();
    assert!(db::repositories::access::revoke_repo_access(
        &database.pool,
        late_writer.id,
        repository.id,
    )
    .await
    .unwrap());
    assert!(remote
        .publish_file_as(&database.pool, repository.id, late_writer.id, &denied)
        .await
        .unwrap_err()
        .is_write_denied());
    assert_eq!(remote.commit("main").await.unwrap().sha, workflow.sha);
    assert_eq!(
        remote
            .blob("main", "denied-after-prepare.txt")
            .await
            .unwrap_err()
            .kind,
        ErrorKind::MissingPath
    );
    compare_reads(&remote, &local, &workflow.sha).await;
    let checkout_root = tempfile::tempdir().unwrap();
    let remote_checkout = checkout_root.path().join("remote");
    let local_checkout = checkout_root.path().join("local");
    remote
        .checkout(&remote_checkout, &workflow.sha)
        .await
        .unwrap();
    local
        .checkout(&local_checkout, &workflow.sha)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(remote_checkout.join("main.rs")).unwrap(),
        std::fs::read(local_checkout.join("main.rs")).unwrap()
    );
    assert_eq!(
        git2::Repository::open(&remote_checkout)
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        workflow.sha
    );

    assert_eq!(
        remote
            .create_branch_as(
                &database.pool,
                repository.id,
                owner.id,
                "feature",
                &workflow.sha
            )
            .await
            .unwrap(),
        local.commit("feature").await.unwrap().sha
    );
    let branch_refs = remote.refs().await.unwrap();
    for alias in ["feature//", "feature//candidate", "/feature", "feature."] {
        assert_eq!(
            remote
                .create_branch_as(
                    &database.pool,
                    repository.id,
                    owner.id,
                    alias,
                    &workflow.sha
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Invalid,
            "create {alias}"
        );
        assert_eq!(
            remote
                .delete_branch_as(&database.pool, repository.id, owner.id, alias)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Invalid,
            "delete {alias}"
        );
        assert_eq!(remote.refs().await.unwrap(), branch_refs, "{alias}");
    }
    let feature = remote
        .save_file_as(
            &database.pool,
            repository.id,
            owner.id,
            &write("feature", "feature.txt", &workflow.sha),
            b"feature\n",
            "Tester",
            "tester@example.invalid",
        )
        .await
        .unwrap();
    equal(&feature, local.commit("feature").await.unwrap());
    equal(
        remote.diff("main", "feature").await.unwrap(),
        local.diff("main", "feature").await.unwrap(),
    );
    equal(
        remote.changed_paths("main", "feature").await.unwrap(),
        local.changed_paths("main", "feature").await.unwrap(),
    );
    for (name, message) in [("light", None), ("annotated", Some("release"))] {
        let tag = remote
            .create_tag_as(
                &database.pool,
                repository.id,
                owner.id,
                name,
                &workflow.sha,
                message,
                "Tester",
                "tester@example.invalid",
            )
            .await
            .unwrap();
        assert!(local
            .tags()
            .await
            .unwrap()
            .iter()
            .any(|local_tag| serde_json::to_value(local_tag).unwrap()
                == serde_json::to_value(&tag).unwrap()));
        let tag_refs = remote.refs().await.unwrap();
        for alias in [
            format!("{name}//candidate"),
            format!("/{name}"),
            format!("{name}."),
        ] {
            assert_eq!(
                remote
                    .create_tag_as(
                        &database.pool,
                        repository.id,
                        owner.id,
                        &alias,
                        &workflow.sha,
                        None,
                        "Tester",
                        "tester@example.invalid"
                    )
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Invalid,
                "create {alias}"
            );
            assert_eq!(
                remote
                    .delete_tag_as(&database.pool, repository.id, owner.id, &alias)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Invalid,
                "delete {alias}"
            );
            assert_eq!(remote.refs().await.unwrap(), tag_refs, "{alias}");
        }
        remote
            .delete_tag_as(&database.pool, repository.id, owner.id, name)
            .await
            .unwrap();
        assert!(!local
            .refs()
            .await
            .unwrap()
            .contains_key(&format!("refs/tags/{name}")));
    }
    let prepared = remote
        .prepare_merge(&merge_request(
            &feature.sha,
            &workflow.sha,
            "Merge",
            "Tester",
            "tester@example.invalid",
        ))
        .await
        .unwrap();
    // Independently prepared commits need not have the same timestamp.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let local_prepared = local
        .prepare_merge(&merge_request(
            &feature.sha,
            &workflow.sha,
            "Merge",
            "Tester",
            "tester@example.invalid",
        ))
        .await
        .unwrap();
    let bare = git2::Repository::open_bare(&storage_path).unwrap();
    let remote_commit = bare
        .find_commit(git2::Oid::from_str(&prepared).unwrap())
        .unwrap();
    let local_commit = bare
        .find_commit(git2::Oid::from_str(&local_prepared).unwrap())
        .unwrap();
    assert_eq!(remote_commit.tree_id(), local_commit.tree_id());
    assert_eq!(
        remote_commit.parent_ids().collect::<Vec<_>>(),
        local_commit.parent_ids().collect::<Vec<_>>()
    );
    assert_eq!(remote_commit.message_bytes(), local_commit.message_bytes());
    assert_eq!(
        remote_commit.author().name_bytes(),
        local_commit.author().name_bytes()
    );
    assert_eq!(
        remote_commit.author().email_bytes(),
        local_commit.author().email_bytes()
    );
    assert_eq!(
        remote_commit.committer().name_bytes(),
        local_commit.committer().name_bytes()
    );
    assert_eq!(
        remote_commit.committer().email_bytes(),
        local_commit.committer().email_bytes()
    );
    drop(remote_commit);
    drop(local_commit);
    drop(bare);
    remote
        .publish_merge_as(
            &database.pool,
            repository.id,
            owner.id,
            "feature",
            "main",
            &feature.sha,
            &workflow.sha,
            &prepared,
            MergeMethod::Merge,
        )
        .await
        .unwrap();
    assert_eq!(local.commit("main").await.unwrap().sha, prepared);
    remote
        .delete_branch_as(&database.pool, repository.id, owner.id, "feature")
        .await
        .unwrap();
    assert!(!local
        .refs()
        .await
        .unwrap()
        .contains_key("refs/heads/feature"));
    remote
        .create_branch_as(&database.pool, repository.id, owner.id, "next", &prepared)
        .await
        .unwrap();
    let next = remote
        .save_file_as(
            &database.pool,
            repository.id,
            owner.id,
            &write("next", "next.txt", &prepared),
            b"next\n",
            "Tester",
            "tester@example.invalid",
        )
        .await
        .unwrap();
    let merge = MergeRequest {
        head_branch: "next".into(),
        base_branch: "main".into(),
        head: next.sha.clone(),
        base: prepared.clone(),
        message: "Merge next".into(),
        name: "Tester".into(),
        email: "tester@example.invalid".into(),
        method: MergeMethod::Merge,
        author_name: None,
        author_email: None,
    };
    let merge_connection = web
        .endpoint()
        .unwrap()
        .connect(storage.endpoint().unwrap().addr(), cluster::ALPN_RPC)
        .await
        .unwrap();
    let merged: String = serde_json::from_value(
        stream_call(
            &merge_connection,
            &Request {
                repository: uuid,
                operation: Operation::Merge {
                    request: merge.clone(),
                },
                actor: Some(owner.id),
                ci_run: None,
            },
        )
        .await
        .result
        .unwrap(),
    )
    .unwrap();
    merge_connection.close(0u32.into(), b"merge complete");
    assert_eq!(merged, local.commit("main").await.unwrap().sha);
    assert!(remote
        .publish_merge_as(
            &database.pool,
            repository.id,
            owner.id,
            &merge.head_branch,
            &merge.base_branch,
            &merge.head,
            &merge.base,
            &merged,
            MergeMethod::Merge
        )
        .await
        .is_err());
    let invalid_head = "0".repeat(40);
    assert_eq!(
        remote
            .prepare_merge(&merge_request(
                &invalid_head,
                &merge.base,
                &merge.message,
                &merge.name,
                &merge.email
            ))
            .await
            .unwrap_err(),
        local
            .prepare_merge(&merge_request(
                &invalid_head,
                &merge.base,
                &merge.message,
                &merge.name,
                &merge.email
            ))
            .await
            .unwrap_err()
    );

    for (remote_error, local_error, kind) in [
        (
            remote.commit("missing").await.unwrap_err(),
            local.commit("missing").await.unwrap_err(),
            ErrorKind::NotFound,
        ),
        (
            remote.blob("main", "missing").await.unwrap_err(),
            local.blob("main", "missing").await.unwrap_err(),
            ErrorKind::MissingPath,
        ),
        (
            remote
                .save_file_as(
                    &database.pool,
                    repository.id,
                    owner.id,
                    &write("main", "bad.txt", &initial.sha),
                    b"stale",
                    "Tester",
                    "tester@example.invalid",
                )
                .await
                .unwrap_err(),
            local
                .save_file_as(
                    &database.pool,
                    repository.id,
                    owner.id,
                    &write("main", "bad.txt", &initial.sha),
                    b"stale",
                    "Tester",
                    "tester@example.invalid",
                )
                .await
                .unwrap_err(),
            ErrorKind::Conflict,
        ),
        (
            remote
                .save_file_as(
                    &database.pool,
                    repository.id,
                    owner.id,
                    &write("main", "../bad", &merged),
                    b"invalid",
                    "Tester",
                    "tester@example.invalid",
                )
                .await
                .unwrap_err(),
            local
                .save_file_as(
                    &database.pool,
                    repository.id,
                    owner.id,
                    &write("main", "../bad", &merged),
                    b"invalid",
                    "Tester",
                    "tester@example.invalid",
                )
                .await
                .unwrap_err(),
            ErrorKind::Invalid,
        ),
    ] {
        assert_eq!(remote_error, local_error);
        assert_eq!(remote_error.kind, kind);
    }
    // Forking stays on the source node and never copies objects onto the web disk.
    let forker = http::Client::login(&router, &database.pool, fork_owner.id).await;
    let fork = forker
        .expect(
            "POST",
            "/api/repos/rpc_owner/project/forks",
            Value::Null,
            StatusCode::CREATED,
        )
        .await;
    let fork = db::repositories::records::get_repository_by_id(
        &database.pool,
        fork["repo"]["id"].as_i64().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(fork.storage_node_id, repository.storage_node_id);
    equal(
        repo_store(&fork, Some(&web.context)).refs().await.unwrap(),
        local.refs().await.unwrap(),
    );
    if let db::DbPool::Postgres(pool) = &database.pool {
        sqlx::query("UPDATE repositories SET is_private=TRUE WHERE id=$1")
            .bind(fork.id)
            .execute(pool)
            .await
            .unwrap();
    }
    let remote_fork = repo_store(&fork, Some(&web.context));
    let local_fork = repo_store(&fork, Some(&storage.context));
    let private_head = remote_fork
        .save_file_as(
            &database.pool,
            fork.id,
            fork_owner.id,
            &write("main", "private.txt", &merged),
            b"remote-private-marker\n",
            "Fork owner",
            "fork@example.invalid",
        )
        .await
        .unwrap();
    let public_head = remote
        .save_file_as(
            &database.pool,
            repository.id,
            owner.id,
            &write("main", "public.txt", &merged),
            b"public only\n",
            "Base owner",
            "base@example.invalid",
        )
        .await
        .unwrap();
    let base_refs = remote.refs().await.unwrap();
    let fork_refs = remote_fork.refs().await.unwrap();
    assert_eq!(
        remote.commit(&private_head.sha).await.unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        remote_fork.commit(&public_head.sha).await.unwrap_err().kind,
        ErrorKind::NotFound
    );
    let diff = remote
        .diff_from(&remote_fork, "main", "main")
        .await
        .unwrap();
    equal(
        &diff,
        local.diff_from(&local_fork, "main", "main").await.unwrap(),
    );
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].filename, "private.txt");
    assert!(diff.files[0].patch.contains("+remote-private-marker\n"));
    let (status, page) = forker
        .page(&format!(
            "/rpc_owner/project/pulls/new?head_repo={}&head=main&base=main&intent=compare",
            fork.id
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("remote-private-marker"));
    assert_eq!(
        remote.commit(&private_head.sha).await.unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        remote_fork.commit(&public_head.sha).await.unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(remote.refs().await.unwrap(), base_refs);
    assert_eq!(remote_fork.refs().await.unwrap(), fork_refs);
    assert_eq!(remote.commit("main").await.unwrap().sha, public_head.sha);

    // Remote pull-head publication uses the same exact-CAS, retryable path as
    // local storage and keeps the authorized snapshot after its fork is deleted.
    remote
        .fetch_pull_head(
            &database.pool,
            repository.id,
            fork.id,
            fork_owner.id,
            &remote_fork,
            98,
            &private_head.sha,
            None,
        )
        .await
        .unwrap();
    remote
        .fetch_pull_head(
            &database.pool,
            repository.id,
            fork.id,
            fork_owner.id,
            &remote_fork,
            98,
            &private_head.sha,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        remote.commit("refs/pull/98/head").await.unwrap().sha,
        private_head.sha
    );
    forker
        .expect(
            "DELETE",
            "/api/repos/rpc_forker/project",
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    assert!(!repo_store(&fork, Some(&storage.context)).exists().await);
    assert_eq!(
        remote.commit("refs/pull/98/head").await.unwrap().sha,
        private_head.sha
    );

    // Even a member cannot ask this node for a repository stored elsewhere.
    let elsewhere_path = web_disk.path().join(format!("{}.git", Uuid::new_v4()));
    let elsewhere = db::repositories::records::create_repository(
        &database.pool,
        owner.id,
        "elsewhere",
        None,
        false,
        elsewhere_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let connection = web
        .endpoint()
        .unwrap()
        .connect(storage.endpoint().unwrap().addr(), cluster::ALPN_RPC)
        .await
        .unwrap();
    let actorless = Request {
        repository: uuid,
        operation: Operation::CreateBranch {
            name: "actorless".into(),
            sha: public_head.sha.clone(),
        },
        actor: None,
        ci_run: None,
    };
    assert!(
        matches!(stream_call(&connection, &actorless).await.result, Err(Failure::Repository(error)) if error.kind == ErrorKind::Invalid)
    );
    assert!(!remote
        .refs()
        .await
        .unwrap()
        .contains_key("refs/heads/actorless"));
    let actorless_pull = Request {
        repository: uuid,
        operation: Operation::FetchPullHead {
            source: uuid,
            number: 99,
            head: public_head.sha.clone(),
            expected_head: None,
        },
        actor: None,
        ci_run: None,
    };
    assert!(matches!(
        stream_call(&connection, &actorless_pull).await.result,
        Err(Failure::Repository(error)) if error.kind == ErrorKind::Invalid
    ));
    assert!(git2::Repository::open_bare(&storage_path)
        .unwrap()
        .find_reference("refs/pull/99/head")
        .is_err());
    let request = Request {
        repository: rpc::repository_uuid(&elsewhere.disk_path).unwrap(),
        operation: Operation::Refs,
        actor: None,
        ci_run: None,
    };
    assert!(
        matches!(stream_call(&connection, &request).await.result, Err(Failure::Repository(error)) if error.kind == ErrorKind::Invalid)
    );
    for (repository, source) in [
        (uuid, rpc::repository_uuid(&elsewhere.disk_path).unwrap()),
        (rpc::repository_uuid(&elsewhere.disk_path).unwrap(), uuid),
    ] {
        let request = Request {
            repository,
            operation: Operation::DiffFrom {
                source,
                base_ref: public_head.sha.clone(),
                head_ref: public_head.sha.clone(),
            },
            actor: None,
            ci_run: None,
        };
        assert!(
            matches!(stream_call(&connection, &request).await.result, Err(Failure::Repository(error)) if error.kind == ErrorKind::Invalid)
        );
    }
    let request = Request {
        repository: uuid,
        operation: Operation::Refs,
        actor: None,
        ci_run: None,
    };
    assert!(stream_call(&connection, &request).await.result.is_ok());
    cluster::revoke(&database.pool, db::cluster::Authority::Operator, "rpc-web")
        .await
        .unwrap();
    assert!(
        matches!(stream_call(&connection, &request).await.result, Err(Failure::Repository(error)) if error.kind == ErrorKind::Invalid)
    );
    connection.close(0u32.into(), b"test complete");
    assert!(remote.refs().await.is_err());
    cluster::approve(&database.pool, db::cluster::Authority::Operator, "rpc-web")
        .await
        .unwrap();
    oversized(&web.context, &storage, false, uuid).await;
    oversized(&web.context, &storage, true, uuid).await;

    // Remove works both while a row exists and after the API deleted its row.
    remote.remove().await.unwrap();
    assert!(!local.exists().await);
    remote.initialize(None, "main").await.unwrap();
    assert!(local.exists().await);
    client
        .expect(
            "DELETE",
            "/api/repos/rpc_owner/project",
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    assert!(!storage_path.exists());
    remote.remove().await.unwrap(); // Already removed is still success.
    storage.shutdown().await;
    let error = remote.refs().await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unavailable);
    assert_eq!(error.message, "storage node unavailable");
    assert!(remote
        .prepare_merge(&merge)
        .await
        .unwrap_err()
        .is_storage_node_unavailable());
    // Recreate only a DB row to test the visible outage through smart HTTP.
    let outage = db::repositories::records::create_repository(
        &database.pool,
        owner.id,
        "offline",
        None,
        false,
        elsewhere_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    if let db::DbPool::Postgres(pool) = &database.pool {
        sqlx::query("UPDATE repositories SET storage_node_id=$1 WHERE id=$2")
            .bind(storage.context.node_id)
            .bind(outage.id)
            .execute(pool)
            .await
            .unwrap();
    }
    let response = client
        .exchange(
            "GET",
            "/rpc_owner/offline.git/info/refs?service=git-upload-pack",
            "",
            Vec::new(),
        )
        .await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.text(), "storage node unavailable");
    let pull = db::pull_requests::records::create_pull_request(
        &database.pool,
        db::pull_requests::records::NewPullRequest {
            repo_id: outage.id,
            head_repo_id: Some(outage.id),
            author_id: owner.id,
            title: "Offline diff",
            body: "",
            head_branch: "topic",
            head_sha: &"1".repeat(40),
            base_branch: "main",
            base_sha: &"2".repeat(40),
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let error = client
        .expect(
            "GET",
            &format!("/api/repos/rpc_owner/offline/pulls/{}/diff", pull.number),
            Value::Null,
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .await;
    assert_eq!(error["message"], "storage node unavailable");
    // An unreachable primary can still have a fresh heartbeat when creation starts.
    db::cluster::record_node_heartbeat(
        &database.pool,
        &reported.public_key,
        reported.version.as_deref().unwrap(),
        reported.addr_hint.as_deref().unwrap(),
        reported.disk_free,
    )
    .await
    .unwrap();
    use tracing::instrument::WithSubscriber;
    let log = tempfile::NamedTempFile::new().unwrap();
    let writer = log.reopen().unwrap();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.try_clone().unwrap())
            .finish(),
    );
    client
        .expect(
            "POST",
            "/api/repos",
            json!({"name":"failed_creation", "is_private":false}),
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .with_subscriber(dispatch.clone())
        .await;
    client
        .expect(
            "DELETE",
            "/api/repos/rpc_owner/offline",
            Value::Null,
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .with_subscriber(dispatch)
        .await;
    assert!(
        db::repositories::records::get_repository_by_id(&database.pool, outage.id)
            .await
            .unwrap()
            .is_none()
    );
    let logged = std::fs::read_to_string(log.path()).unwrap();
    for message in [
        "failed repository initialization storage cleanup",
        "deleted repository storage remains",
    ] {
        let line = logged
            .lines()
            .find(|line| line.contains(message))
            .expect("cleanup failure was logged");
        assert!(
            line.contains(&format!(
                "storage_node_id=Some({})",
                storage.context.node_id
            )),
            "{line}"
        );
        assert!(line.contains("error=storage node unavailable"), "{line}");
        let path = line
            .split("disk_path=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
        rpc::repository_uuid(path).expect("the logged path preserves the repository UUID");
        if message == "deleted repository storage remains" {
            assert_eq!(path, outage.disk_path);
            assert!(line.contains(&format!("repo_id={}", outage.id)), "{line}");
        }
    }
    assert!(db::repositories::records::get_repository(
        &database.pool,
        "rpc_owner",
        "failed_creation"
    )
    .await
    .unwrap()
    .is_none());
    web.shutdown().await;
}

//! Code search over a repository stored on another node. The web node asks the storage node for
//! the matching lines through the repository RPC, as it does for every other read of that
//! repository.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::native_database as native;

use codeyard::backend::{
    cluster,
    db::{self, repositories::creation::RepositoryCreationOwner},
    repositories::{
        lifecycle::provision,
        store::{repo_store, ErrorKind, FileWrite},
    },
};
use std::path::Path;

async fn start_node(
    database: &native::PostgresDatabase,
    root: &Path,
    name: &str,
    roles: &str,
) -> cluster::Cluster {
    let settings = cluster::Settings::from_lookup(&|key| match key {
        "IROH_BIND" => Some("127.0.0.1:0".into()),
        "NODE_NAME" => Some(name.into()),
        _ => None,
    })
    .unwrap()
    .unwrap();
    let node = cluster::start(
        &database.pool,
        &root.join(format!("{name}-state")),
        &root.join(format!("{name}-repos")),
        &roles.parse().unwrap(),
        settings,
    )
    .await
    .unwrap();
    node.publish().await.unwrap();
    node
}

#[tokio::test]
async fn code_search_reads_a_repository_stored_on_another_node() {
    let database = native::PostgresDatabase::new().await;
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let web = start_node(&database, root.path(), "web-1", "web").await;
    let storage = start_node(&database, root.path(), "store-1", "storage").await;
    let owner = db::users::accounts::create_user(&database.pool, "search_remote", None)
        .await
        .unwrap();
    let repo = provision(
        (&database.pool, Some(&web.context)),
        root.path(),
        RepositoryCreationOwner::personal(owner.id),
        "remote",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(repo.storage_node_id, Some(storage.context.node_id));

    let store = repo_store(&repo, Some(&web.context));
    store
        .save_file_as(
            &database.pool,
            repo.id,
            owner.id,
            &FileWrite {
                branch: "main".into(),
                path: "src/lib.rs".into(),
                expected_revision: "unborn".into(),
                message: "Add src/lib.rs".into(),
                creating: true,
                deleting: false,
            },
            b"fn needle_search() {}\n",
            "Alice",
            "alice@example.invalid",
        )
        .await
        .unwrap();
    let matches = store
        .search_code("main", "NEEDLE_search", 0, 5)
        .await
        .unwrap();
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0].path, "src/lib.rs");
    assert_eq!(matches[0].line_number, 1);
    assert_eq!(matches[0].line, "fn needle_search() {}");
}

#[tokio::test]
async fn remote_file_lists_and_commit_walks_keep_size_limits_and_deadlines() {
    use codeyard::backend::repositories::store::{CommitFilter, RepoStore};
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let web = start_node(&database, root.path(), "large-web", "web").await;
    let storage = start_node(&database, root.path(), "large-store", "storage").await;
    let owner = db::users::accounts::create_user(&database.pool, "large_remote", None)
        .await
        .unwrap();
    let repository = provision(
        (&database.pool, Some(&web.context)),
        root.path(),
        RepositoryCreationOwner::personal(owner.id),
        "large",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let uuid = cluster::rpc::repository_uuid(&repository.disk_path).unwrap();
    let path = storage.context.repos_path.join(format!("{uuid}.git"));
    let bare = git2::Repository::open_bare(&path).unwrap();
    let blob = bare.blob(b"small").unwrap();
    let mut leaves = bare.treebuilder(None).unwrap();
    for index in 0..10_000 {
        leaves
            .insert(format!("{index:05}{}", "x".repeat(145)), blob, 0o100644)
            .unwrap();
    }
    let mut tree = leaves.write().unwrap();
    for name in ["a", "b", "c"] {
        let mut parent = bare.treebuilder(None).unwrap();
        parent.insert(name.repeat(250), tree, 0o040000).unwrap();
        tree = parent.write().unwrap();
    }
    let signature = git2::Signature::now("Tester", "tester@example.invalid").unwrap();
    let empty = bare
        .find_tree(bare.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let base = bare
        .commit(None, &signature, &signature, "base", &empty, &[])
        .unwrap();
    let tree = bare.find_tree(tree).unwrap();
    let message = "m".repeat(220 * 1024);
    let mut head = base;
    for _ in 0..40 {
        let parent = bare.find_commit(head).unwrap();
        head = bare
            .commit(None, &signature, &signature, &message, &tree, &[&parent])
            .unwrap();
    }
    bare.reference("refs/heads/main", head, true, "fixture")
        .unwrap();
    let local = RepoStore::from_disk_path(path.to_str().unwrap());
    let remote = repo_store(&repository, Some(&web.context));
    let paths = remote.file_paths("main", 10_000).await.unwrap();
    assert_eq!(
        serde_json::to_value(&paths).unwrap(),
        serde_json::to_value(local.file_paths("main", 10_000).await.unwrap()).unwrap()
    );
    assert!(paths.truncated);
    assert!(serde_json::to_vec(&paths).unwrap().len() < 1024 * 1024 + 128);
    let local_error = local
        .commits_between(&base.to_string(), &head.to_string(), 100)
        .await
        .unwrap_err();
    let remote_error = remote
        .commits_between(&base.to_string(), &head.to_string(), 100)
        .await
        .unwrap_err();
    assert!(local_error.is_response_too_large());
    assert_eq!(remote_error.kind, local_error.kind);
    assert_eq!(remote_error.message, local_error.message);
    for offset in [0, 1] {
        let expired = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            remote.commits_until("main", offset, 10, &CommitFilter::default(), expired),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unavailable);
    }
    storage.shutdown().await;
    web.shutdown().await;
}

/// A repository on a storage node that is down is an error for the search, not a repository with
/// nothing to find. The search page reports such a repository and keeps the rest.
#[tokio::test]
async fn code_search_on_an_unreachable_storage_node_is_an_error_not_an_empty_repository() {
    let database = native::PostgresDatabase::new().await;
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let web = start_node(&database, root.path(), "web-1", "web").await;
    let storage = start_node(&database, root.path(), "store-1", "storage").await;
    let owner = db::users::accounts::create_user(&database.pool, "search_outage", None)
        .await
        .unwrap();
    let repo = provision(
        (&database.pool, Some(&web.context)),
        root.path(),
        RepositoryCreationOwner::personal(owner.id),
        "outage",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let store = repo_store(&repo, Some(&web.context));
    store
        .save_file_as(
            &database.pool,
            repo.id,
            owner.id,
            &FileWrite {
                branch: "main".into(),
                path: "src/lib.rs".into(),
                expected_revision: "unborn".into(),
                message: "Add src/lib.rs".into(),
                creating: true,
                deleting: false,
            },
            b"fn needle_search() {}\n",
            "Alice",
            "alice@example.invalid",
        )
        .await
        .unwrap();
    storage.shutdown().await;
    let error = store
        .search_code("main", "needle_search", 0, 5)
        .await
        .expect_err("the storage node is down");
    assert_ne!(error.kind, ErrorKind::NotFound, "{error:?}");
}

#[tokio::test]
async fn content_search_allows_transport_fallback_and_keeps_completed_repositories() {
    use crate::support::{fake_webui_bundle, pull_requests::Client};
    use codeyard::{
        backend::{api::create_router, app::AppState, ci::CiConfig, storage::ObjectStore},
        webui::{app::Page, pages::discovery},
    };
    use std::{sync::Arc, time::Duration};
    fake_webui_bundle::install();
    let database = native::PostgresDatabase::new().await;
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let web = start_node(&database, root.path(), "search-web", "web").await;
    let storage = start_node(&database, root.path(), "search-storage", "storage").await;
    let owner = db::users::accounts::create_user(&database.pool, "deadline_owner", None)
        .await
        .unwrap();
    let remote = provision(
        (&database.pool, Some(&web.context)),
        root.path(),
        RepositoryCreationOwner::personal(owner.id),
        "remote",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let local = provision(
        (&database.pool, None),
        root.path(),
        RepositoryCreationOwner::personal(owner.id),
        "healthy",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    for repo in [&remote, &local] {
        repo_store(repo, Some(&web.context))
            .save_file_as(
                &database.pool,
                repo.id,
                owner.id,
                &FileWrite {
                    branch: "main".into(),
                    path: "notes.txt".into(),
                    expected_revision: "unborn".into(),
                    message: "needle commit".into(),
                    creating: true,
                    deleting: false,
                },
                b"needle\n",
                "Owner",
                "owner@example.test",
            )
            .await
            .unwrap();
    }
    let state = create_router(AppState::new(
        database.pool.clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        Some(web.context.clone()),
    ));
    let client = Client::anonymous(&state);
    let db::DbPool::Postgres(raw) = &database.pool else {
        unreachable!()
    };
    let node = db::cluster::get_node_by_id(&database.pool, storage.context.node_id)
        .await
        .unwrap()
        .unwrap();
    let mut address: cluster::NodeAddress =
        serde_json::from_str(node.addr_hint.as_deref().unwrap()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    address.tcp = vec![listener.local_addr().unwrap()];
    let silent = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    for kind in ["code", "commits"] {
        sqlx::query("UPDATE nodes SET addr_hint=$1 WHERE id=$2")
            .bind(serde_json::to_string(&address).unwrap())
            .bind(storage.context.node_id)
            .execute(raw)
            .await
            .unwrap();
        let (status, page) = client
            .model(&format!(
                "/search?q=needle%20repo:deadline_owner/remote&type={kind}"
            ))
            .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let Page::Discovery(discovery::Page::Search(model)) = page else {
            panic!("search")
        };
        assert_eq!(model.unsearched, 0);
        assert_eq!(
            model.code.len() + model.commits.len(),
            1,
            "TCP failure must leave time for iroh"
        );
    }
    silent.abort();
    let _ = silent.await;
    // A real database wait stalls only the remote repository's transport lookup. The healthy
    // local repository finishes behind it in the ordered search batch.
    sqlx::query("UPDATE repositories SET updated_at=NOW()+INTERVAL '1 hour' WHERE id=$1")
        .bind(remote.id)
        .execute(raw)
        .await
        .unwrap();
    let mut gate = raw.begin().await.unwrap();
    sqlx::query("LOCK TABLE nodes IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *gate)
        .await
        .unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(37)).await;
        gate.rollback().await.unwrap();
    });
    let (status, page) = tokio::time::timeout(
        Duration::from_secs(45),
        client.model("/search?q=needle&type=code"),
    )
    .await
    .unwrap();
    assert_eq!(status, axum::http::StatusCode::OK);
    let Page::Discovery(discovery::Page::Search(model)) = page else {
        panic!("search")
    };
    assert!(model.code.iter().any(|hit| hit.repo == "healthy"));
    assert_eq!(model.unsearched, 1);
    release.await.unwrap();
    storage.shutdown().await;
    web.shutdown().await;
}

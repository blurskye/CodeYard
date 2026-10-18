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

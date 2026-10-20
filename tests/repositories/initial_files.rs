//! A new repository's first commit holds the README, .gitignore and license its creator chose,
//! on disposable SQLite and PostgreSQL data.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use chrono::{Datelike, Utc};
use codeyard::backend::{
    db,
    repositories::{
        lifecycle::{provision_with_initial_files, InitialFiles},
        store::repo_store,
        templates,
    },
};
use database::Database;

async fn contract(pg: bool) {
    let f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repository = provision_with_initial_files(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "starter",
        Some("A starter project"),
        false,
        InitialFiles {
            readme: true,
            gitignore: templates::gitignore("Rust"),
            license: templates::license("MIT"),
        },
    )
    .await
    .unwrap();
    let store = repo_store(&repository, None);
    let branch = repository.default_branch.as_str();

    let commit = store.commit(branch).await.unwrap();
    assert!(commit.parents.is_empty());
    assert_eq!(commit.message.trim_end(), "Initial commit");
    assert_eq!(commit.author_name, "alice");
    assert_eq!(store.commit_count().await.unwrap(), 1);

    let mut paths = store.file_paths(branch, 100).await.unwrap().paths;
    paths.sort();
    assert_eq!(paths, [".gitignore", "LICENSE", "README.md"]);
    let readme = store.blob(branch, "README.md").await.unwrap();
    assert_eq!(readme.content, "# starter\n\nA starter project\n");
    let ignore = store.blob(branch, ".gitignore").await.unwrap();
    assert!(ignore.content.contains("/target/"));
    let license = store.blob(branch, "LICENSE").await.unwrap();
    let year = Utc::now().year();
    assert!(license.content.starts_with("MIT License\n\nCopyright (c) "));
    assert!(license
        .content
        .contains(&format!("Copyright (c) {year} alice\n")));

    let oversized = "x".repeat(26 * 1024 * 1024);
    let failure = provision_with_initial_files(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "preserved",
        Some(&oversized),
        false,
        InitialFiles {
            readme: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(failure.message.contains("preserved"));
    let preserved = db::repositories::records::get_repository(f.pool(), "alice", "preserved")
        .await
        .unwrap()
        .unwrap();
    assert!(std::path::Path::new(&preserved.disk_path).exists());
}

#[tokio::test]
async fn sqlite_new_repository_starts_with_its_chosen_files() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_new_repository_starts_with_its_chosen_files() {
    contract(true).await;
}

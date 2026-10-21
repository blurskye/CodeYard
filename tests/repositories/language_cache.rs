use crate::support::database::Database;
use codeyard::backend::{
    db,
    repositories::{languages, lifecycle, store::repo_store},
};
use std::time::Duration;

async fn empty_language_results_are_cached_by_revision(pg: bool) {
    let mut f = Database::new(pg).await;
    let owner = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repository = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "assets",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let git = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let mut builder = git.treebuilder(None).unwrap();
    builder
        .insert("image.png", git.blob(b"image").unwrap(), 0o100644)
        .unwrap();
    let signature = git2::Signature::now("Test", "test@example.test").unwrap();
    let tip = git
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Assets",
            &git.find_tree(builder.write().unwrap()).unwrap(),
            &[],
        )
        .unwrap();
    let store = repo_store(&repository, None);
    assert!(
        languages::compute_and_store(f.pool(), &store, &repository, "main")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db::repository_stats::language_revision(f.pool(), repository.id, "main")
            .await
            .unwrap(),
        Some(tip.to_string())
    );
    // A cache hit must not attempt another replacement transaction.
    match &f {
        Database::Sqlite(database) => {
            let tx = database.raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
            let cached = tokio::time::timeout(
                Duration::from_secs(2),
                languages::compute_and_store(f.pool(), &store, &repository, "main"),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(cached.is_empty());
            tx.rollback().await.unwrap();
        }
        Database::Postgres(database) => {
            let mut tx = database.raw.begin().await.unwrap();
            sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
                .bind(repository.id)
                .execute(&mut *tx)
                .await
                .unwrap();
            let cached = tokio::time::timeout(
                Duration::from_secs(2),
                languages::compute_and_store(f.pool(), &store, &repository, "main"),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(cached.is_empty());
            tx.rollback().await.unwrap();
        }
    }
    builder
        .insert("lib.rs", git.blob(b"fn main() {}\n").unwrap(), 0o100644)
        .unwrap();
    let next = git
        .commit(
            Some("refs/heads/topic"),
            &signature,
            &signature,
            "Source",
            &git.find_tree(builder.write().unwrap()).unwrap(),
            &[&git.find_commit(tip).unwrap()],
        )
        .unwrap();
    git.set_head("refs/heads/topic").unwrap();
    db::repositories::records::set_repository_default_branch(
        f.pool(),
        owner,
        repository.id,
        "topic",
    )
    .await
    .unwrap();
    let current = f.repository(repository.id).await;
    let result = languages::compute_and_store(f.pool(), &store, &current, "topic")
        .await
        .unwrap();
    assert_eq!(result[0].language, "Rust");
    assert_eq!(
        db::repository_stats::language_revision(f.pool(), repository.id, "topic")
            .await
            .unwrap(),
        Some(next.to_string())
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_empty_language_results_are_cached_by_revision() {
    empty_language_results_are_cached_by_revision(false).await;
}

#[tokio::test]
async fn postgres_empty_language_results_are_cached_by_revision() {
    empty_language_results_are_cached_by_revision(true).await;
}

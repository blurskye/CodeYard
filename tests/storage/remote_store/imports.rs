//! Source imports are publishers, not a way to import somebody else's server attestations.
use super::*;
use codeyard::backend::repositories::imports::{GitImport, ImportCredential};

#[tokio::test]
async fn remote_imports_attest_the_importer_and_reject_foreign_provenance() {
    let _session_cap = lock_git_session_cap().await;
    bundle::install();
    let database = native::PostgresDatabase::new().await;
    codeyard_db_cluster::cluster::enable_cluster(&database.pool)
        .await
        .unwrap();
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let web = cluster::start(
        &database.pool,
        &root.path().join("web-state"),
        &root.path().join("web-repos"),
        &"web,ssh".parse().unwrap(),
        settings("import-web"),
    )
    .await
    .unwrap();
    web.publish().await.unwrap();
    let storage = cluster::start(
        &database.pool,
        &root.path().join("storage-state"),
        &root.path().join("storage-repos"),
        &"storage".parse().unwrap(),
        settings("import-storage"),
    )
    .await
    .unwrap();
    storage.publish().await.unwrap();
    let owner = db::users::accounts::create_user(&database.pool, "import_publisher", None)
        .await
        .unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (&database.pool, Some(&web.context)),
        &web.context.repos_path,
        db::repositories::creation::RepositoryCreationOwner::personal(owner.id),
        "imported",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(repository.storage_node_id, Some(storage.context.node_id));
    let session = native::token();
    db::users::sessions::create_session(
        &database.pool,
        &session,
        owner.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let token = db::users::tokens::create_token(
        &database.pool,
        owner.id,
        &session,
        "import",
        &"a".repeat(64),
        "write",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let generation = Uuid::new_v4().to_string();
    db::source_imports::start(
        &database.pool,
        owner.id,
        repository.id,
        "https://example.invalid/source.git",
        &generation,
    )
    .await
    .unwrap();
    let (tip, pack) = {
        let source = git2::Repository::init_bare(root.path().join("source")).unwrap();
        let blob = source.blob(b"imported\n").unwrap();
        let mut tree = source.treebuilder(None).unwrap();
        tree.insert("README", blob, 0o100644).unwrap();
        let tree = source.find_tree(tree.write().unwrap()).unwrap();
        let author = git2::Signature::now("Original author", "original@example.invalid").unwrap();
        let tip = source
            .commit(None, &author, &author, "Original", &tree, &[])
            .unwrap();
        let mut builder = source.packbuilder().unwrap();
        builder.insert_commit(tip).unwrap();
        let mut pack = git2::Buf::new();
        builder.write_buf(&mut pack).unwrap();
        (tip.to_string(), pack.to_vec())
    };
    let remote = repo_store(&repository, Some(&web.context));
    let local = repo_store(&repository, Some(&storage.context));
    let reference = "refs/heads/main";
    let mut import = GitImport {
        generation,
        credential: ImportCredential::Token(token.id),
        refs: [(reference.to_owned(), tip.clone())].into(),
        default_branch: Some("main".into()),
    };
    for forbidden in ["refs/codeyard", "refs/codeyard/publications/foreign"] {
        let mut forged = import.clone();
        forged.refs.insert(forbidden.into(), tip.clone());
        assert!(remote
            .import_git_as(
                &database.pool,
                repository.id,
                owner.id,
                forged,
                pack.clone()
            )
            .await
            .is_err());
        assert!(local.mirror_refs().await.unwrap().is_empty());
    }
    remote
        .import_git_as(
            &database.pool,
            repository.id,
            owner.id,
            import.clone(),
            pack.clone(),
        )
        .await
        .unwrap();
    let proof = remote
        .branch_publication(repository.id, reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        proof.publication.actor,
        git::PublicationActor::User(owner.id)
    );
    assert_eq!(proof.publication.new, tip);
    assert_eq!(proof.publication.old, "0".repeat(40));
    assert_eq!(proof.publication.repository_id, repository.id);
    assert_eq!(proof.publication.reference, reference);
    assert_eq!(
        local
            .branch_publication(repository.id, reference)
            .await
            .unwrap(),
        Some(proof.clone())
    );
    assert_eq!(remote.refs_from_primary().await.unwrap()[reference], tip);
    let contributors = remote.contributors().await.unwrap();
    assert_eq!(contributors.len(), 1);
    assert_eq!(contributors[0].name, "Original author");
    assert_eq!(contributors[0].commits, 1);
    // A refused subsequent import does not generate a different publisher epoch.
    import.generation = Uuid::new_v4().to_string();
    assert!(remote
        .import_git_as(&database.pool, repository.id, owner.id, import, pack)
        .await
        .is_err());
    assert_eq!(
        remote
            .branch_publication(repository.id, reference)
            .await
            .unwrap(),
        Some(proof)
    );
    local.remove().await.unwrap();
    storage.shutdown().await;
    web.shutdown().await;
    database.pool.close().await;
}

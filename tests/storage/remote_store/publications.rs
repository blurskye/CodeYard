//! Real publishers, primary-only reads and no-op/ABA identity parity across storage RPC.
use super::*;

#[tokio::test]
async fn branch_provenance_tracks_publishers_not_authors_and_survives_remote_noops() {
    let _session_cap = lock_git_session_cap().await;
    bundle::install();
    let database = native::PostgresDatabase::new().await;
    codeyard_db_cluster::cluster::enable_cluster(&database.pool)
        .await
        .unwrap();
    codeyard_db_cluster::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let web = cluster::start(
        &database.pool,
        &root.path().join("web-state"),
        &root.path().join("web-repos"),
        &"web,ssh".parse().unwrap(),
        settings("publication-web"),
    )
    .await
    .unwrap();
    web.publish().await.unwrap();
    let storage = cluster::start(
        &database.pool,
        &root.path().join("storage-state"),
        &root.path().join("storage-repos"),
        &"storage".parse().unwrap(),
        settings("publication-storage"),
    )
    .await
    .unwrap();
    storage.publish().await.unwrap();
    let owner = codeyard_db_identity::users::accounts::create_user(
        &database.pool,
        "publication_owner",
        None,
    )
    .await
    .unwrap();
    let writer = codeyard_db_identity::users::accounts::create_user(
        &database.pool,
        "publication_writer",
        None,
    )
    .await
    .unwrap();
    let outsider = codeyard_db_identity::users::accounts::create_user(
        &database.pool,
        "publication_outsider",
        None,
    )
    .await
    .unwrap();
    let repository = codeyard_repos_core::repositories::lifecycle::provision(
        (&database.pool, Some(&web.context)),
        &web.context.repos_path,
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner.id),
        "project",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(repository.storage_node_id, Some(storage.context.node_id));
    codeyard_identity::repository_access::change_collaborator(
        &database.pool,
        owner.id,
        repository.id,
        &writer.username,
        Some(codeyard_db_core::models::AccessLevel::Write),
    )
    .await
    .unwrap();
    let remote = repo_store(&repository, Some(&web.context));
    let local = repo_store(&repository, Some(&storage.context));
    let reference = "refs/heads/main";
    assert!(remote
        .branch_publication(repository.id, reference)
        .await
        .unwrap()
        .is_none());
    let initial = remote
        .save_file_as(
            &database.pool,
            repository.id,
            owner.id,
            &write("main", "main.rs", "unborn"),
            b"fn main() {}\n",
            "Code author",
            "code@example.invalid",
        )
        .await
        .unwrap();
    let first = remote
        .branch_publication(repository.id, reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        local
            .branch_publication(repository.id, reference)
            .await
            .unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        first.publication.actor,
        git::PublicationActor::User(owner.id)
    );
    assert_eq!(first.publication.new, initial.sha);
    assert_eq!(first.publication.reference, reference);
    remote
        .update_reference_as(
            &database.pool,
            repository.id,
            writer.id,
            reference,
            &initial.sha,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        remote
            .branch_publication(repository.id, reference)
            .await
            .unwrap(),
        Some(first.clone())
    );
    let changed = remote
        .save_file_as(
            &database.pool,
            repository.id,
            writer.id,
            &write("main", "changed.rs", &initial.sha),
            b"fn main() { println!(\"changed\"); }\n",
            "Another code author",
            "another@example.invalid",
        )
        .await
        .unwrap();
    let second = remote
        .branch_publication(repository.id, reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        second.publication.actor,
        git::PublicationActor::User(writer.id)
    );
    assert_eq!(second.publication.old, initial.sha);
    assert_eq!(second.publication.new, changed.sha);
    assert_ne!(second.publication.id, first.publication.id);
    assert_eq!(
        local
            .branch_publication(repository.id, reference)
            .await
            .unwrap(),
        Some(second.clone())
    );
    assert!(remote
        .save_file_as(
            &database.pool,
            repository.id,
            owner.id,
            &write("main", "stale.txt", &initial.sha),
            b"stale\n",
            "Code author",
            "code@example.invalid",
        )
        .await
        .is_err());
    assert!(remote
        .update_reference_as(
            &database.pool,
            repository.id,
            outsider.id,
            reference,
            &initial.sha,
            true
        )
        .await
        .is_err());
    assert_eq!(
        remote
            .branch_publication(repository.id, reference)
            .await
            .unwrap(),
        Some(second.clone())
    );
    remote
        .update_reference_as(
            &database.pool,
            repository.id,
            owner.id,
            reference,
            &initial.sha,
            true,
        )
        .await
        .unwrap();
    let returned = remote
        .branch_publication(repository.id, reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        returned.publication.actor,
        git::PublicationActor::User(owner.id)
    );
    assert_eq!(returned.publication.new, first.publication.new);
    assert_ne!(returned.publication.id, first.publication.id);
    assert_ne!(returned.publication.id, second.publication.id);
    let contributors = remote.contributors().await.unwrap();
    assert_eq!(contributors.len(), 1);
    assert_eq!(contributors[0].name, "Code author");
    assert_eq!(contributors[0].commits, 1);
    let advertised = remote.advertise_refs(GitService::UploadPack).await.unwrap();
    assert!(!String::from_utf8_lossy(&advertised).contains("refs/codeyard/"));
    assert!(remote
        .refs_from_primary()
        .await
        .unwrap()
        .keys()
        .all(|name| !name.starts_with("refs/codeyard/")));
    assert!(!Operation::BranchPublication {
        repository_id: repository.id,
        reference: reference.into()
    }
    .is_read_only());
    // The shared base's pull-branch updater is also a real branch publisher, even when
    // it fast-forwards to a commit originally published by a different user.
    remote
        .create_branch_as(
            &database.pool,
            repository.id,
            owner.id,
            "upstream",
            &changed.sha,
        )
        .await
        .unwrap();
    let pull = codeyard_db_repos::pull_requests::records::create_pull_request(
        &database.pool,
        codeyard_db_repos::pull_requests::records::NewPullRequest {
            repo_id: repository.id,
            head_repo_id: Some(repository.id),
            author_id: owner.id,
            issue: None,
            maintainer_can_modify: false,
            title: "Update publication",
            body: "",
            head_branch: "main",
            head_sha: &initial.sha,
            base_branch: "upstream",
            base_sha: &changed.sha,
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let mut request = codeyard_repos_core::store_types::PullBranchUpdate {
        pull_request_id: pull.id,
        base_branch: "upstream".into(),
        branch: "main".into(),
        head: initial.sha.clone(),
        base: changed.sha.clone(),
        rebase: false,
        name: "Branch updater".into(),
        email: "updater@example.invalid".into(),
        token: None,
    };
    let updated = remote
        .update_pull_branch_as(
            &database.pool,
            repository.id,
            owner.id,
            (repository.id, &remote),
            &request,
        )
        .await
        .unwrap();
    let publication = remote
        .branch_publication(repository.id, reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        publication.publication.actor,
        git::PublicationActor::User(owner.id)
    );
    assert_eq!(publication.publication.old, initial.sha);
    assert_eq!(publication.publication.new, updated);
    assert_ne!(publication.publication.id, returned.publication.id);
    assert_eq!(
        local
            .branch_publication(repository.id, reference)
            .await
            .unwrap(),
        Some(publication.clone())
    );
    request.head = updated.clone();
    assert_eq!(
        remote
            .update_pull_branch_as(
                &database.pool,
                repository.id,
                owner.id,
                (repository.id, &remote),
                &request
            )
            .await
            .unwrap(),
        updated
    );
    assert_eq!(
        remote
            .branch_publication(repository.id, reference)
            .await
            .unwrap(),
        Some(publication)
    );
    storage.shutdown().await;
    web.shutdown().await;
    database.pool.close().await;
}

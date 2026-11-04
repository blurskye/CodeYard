//! Association parity gets its own fixture so it does not enlarge the broad RPC contract future.
use super::*;
use codeyard_app::repositories::commit_pulls::associated_pulls;
use codeyard_app::repositories::commit_pulls::CommitPullQuery;

#[tokio::test]
async fn associated_pulls_use_primary_storage_without_changing_refs_or_metadata() {
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
        settings("association-web"),
    )
    .await
    .unwrap();
    web.publish().await.unwrap();
    let storage = cluster::start(
        &database.pool,
        &root.path().join("storage-state"),
        &root.path().join("storage-repos"),
        &"storage".parse().unwrap(),
        settings("association-storage"),
    )
    .await
    .unwrap();
    storage.publish().await.unwrap();
    let owner =
        codeyard_db_identity::users::accounts::create_user(&database.pool, "rpc_owner", None)
            .await
            .unwrap();
    let repository = codeyard_app::repositories::lifecycle::provision(
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
    let remote = repo_store(&repository, Some(&web.context));
    let router = create_router(AppState::new(
        database.pool.clone(),
        web.context.repos_path.clone(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        Some(web.context.clone()),
    ));
    let client = http::Client::login(&router, &database.pool, owner.id).await;
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
    remote
        .create_branch_as(
            &database.pool,
            repository.id,
            owner.id,
            "association-rpc",
            &initial.sha,
        )
        .await
        .unwrap();
    let association = remote
        .save_file_as(
            &database.pool,
            repository.id,
            owner.id,
            &write("association-rpc", "association.txt", &initial.sha),
            b"remote association\n",
            "Tester",
            "tester@example.invalid",
        )
        .await
        .unwrap();
    let pull = client
        .expect(
            "POST",
            "/api/repos/rpc_owner/project/pulls",
            json!({
                "title":"Remote association", "head_branch":"association-rpc", "base_branch":"main"
            }),
            StatusCode::CREATED,
        )
        .await;
    let number = i32::try_from(pull["number"].as_i64().unwrap()).unwrap();
    let pull = codeyard_db_repos::pull_requests::records::get_pull_request(
        &database.pool,
        repository.id,
        number,
    )
    .await
    .unwrap()
    .unwrap();
    let refs = remote.refs_from_primary().await.unwrap();
    for context in [&web.context, &storage.context] {
        let page = associated_pulls(
            &database.pool,
            Some(context),
            CommitPullQuery {
                repository_id: repository.id,
                actor: Some(owner.id),
                reference: &association.sha,
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
        assert_eq!(page.commit.sha, association.sha);
        assert_eq!(
            page.pulls.iter().map(|pull| pull.id).collect::<Vec<_>>(),
            vec![pull.id]
        );
        assert!(!page.has_more);
        let page = associated_pulls(
            &database.pool,
            Some(context),
            CommitPullQuery {
                repository_id: repository.id,
                actor: Some(owner.id),
                reference: &initial.sha,
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
        assert!(page.pulls.is_empty());
    }
    assert_eq!(remote.refs_from_primary().await.unwrap(), refs);
    assert_eq!(
        codeyard_db_repos::pull_requests::records::get_pull_request(
            &database.pool,
            repository.id,
            number
        )
        .await
        .unwrap()
        .unwrap()
        .updated_at,
        pull.updated_at
    );
    client.expect("PUT", &format!("/api/repos/rpc_owner/project/pulls/{number}"), json!({
        "expected_state":"open", "expected_updated":pull.updated_at.to_rfc3339(), "state":"closed"
    }), StatusCode::OK).await;
    remote
        .delete_branch_as(&database.pool, repository.id, owner.id, "association-rpc")
        .await
        .unwrap();
    storage.shutdown().await;
    web.shutdown().await;
    database.pool.close().await;
}

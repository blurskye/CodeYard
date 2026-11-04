use super::*;
use axum::{http::StatusCode, Router};
use codeyard::backend::repositories::deploy_keys;
const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
pub(super) async fn contract(
    f: &Database,
    router: &Router,
    browser: &Client,
    owner: i64,
    orgs: &[organizations::OrganizationMetadata],
    repositories: (&db::models::Repository, &db::models::Repository),
    write: Option<&str>,
) {
    let (first, second) = repositories;
    let path = "/api/v3/orgs/acme";
    let keys = "/api/repos/acme/first/keys";
    let (status, _) = browser
        .api(
            "POST",
            keys,
            json!({"title":"Blocked key","key":KEY,"can_write":true}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    request(
        router,
        UPDATE,
        "PATCH",
        path,
        write,
        json!({"deploy_keys_enabled_for_repositories":true}),
        200,
    )
    .await;
    let (status, key) = browser
        .api(
            "POST",
            keys,
            json!({"title":"Deployment","key":KEY,"can_write":true}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{key}");
    let id = key["id"].as_i64().unwrap();
    assert_eq!(
        deploy_keys::authorize(f.pool(), id, first, true)
            .await
            .unwrap()
            .1,
        AccessLevel::Write
    );
    request(
        router,
        UPDATE,
        "PATCH",
        path,
        write,
        json!({"deploy_keys_enabled_for_repositories":false}),
        200,
    )
    .await;
    for write in [false, true] {
        assert!(deploy_keys::authorize(f.pool(), id, first, write)
            .await
            .is_err());
    }
    assert!(
        db::deploy_keys::get_deploy_key(f.pool(), id)
            .await
            .unwrap()
            .is_some(),
        "disabling is not deletion"
    );
    let (status, listed) = browser.api("GET", keys, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    request(
        router,
        UPDATE,
        "PATCH",
        path,
        write,
        json!({"deploy_keys_enabled_for_repositories":true}),
        200,
    )
    .await;
    assert!(deploy_keys::authorize(f.pool(), id, first, true)
        .await
        .is_ok());
    let target =
        data::Changes::parse(&json!({"deploy_keys_enabled_for_repositories":false})).unwrap();
    applied(
        data::change(f.pool(), owner, orgs[1].id, &target)
            .await
            .unwrap(),
    );
    let moved = db::repositories::records::transfer_repository(
        f.pool(),
        owner,
        first.id,
        db::repositories::creation::RepositoryTarget::Organization(orgs[1].id),
    )
    .await
    .unwrap();
    assert!(
        deploy_keys::authorize(f.pool(), id, first, true)
            .await
            .is_err(),
        "a cached old organization must not bypass the destination key policy"
    );
    assert!(deploy_keys::authorize(f.pool(), id, &moved, true)
        .await
        .is_err());
    let (status, _) = browser
        .api(
            "DELETE",
            &format!("/api/repos/foreign/first/keys/{id}"),
            Value::Null,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "disabled keys remain removable"
    );
    request(router,UPDATE,"PATCH",path,write,json!({"default_repository_permission":"read","members_can_fork_private_repositories":false}),200).await;
    browser.expect("POST","/api/repos/acme/second/contents/source.txt",json!({"content":"U291cmNl","message":"Initial API commit","branch":"main","expected_revision":"unborn"}),StatusCode::CREATED).await;
    let member = db::users::accounts::get_user_by_username(f.pool(), "member")
        .await
        .unwrap()
        .unwrap();
    let client = Client::login(router, f.pool(), member.id).await;
    assert_eq!(
        db::repositories::access::get_user_repo_access(f.pool(), member.id, second.id)
            .await
            .unwrap(),
        AccessLevel::Read
    );
    let (status, _) = client
        .api("POST", "/api/repos/acme/second/forks", json!({}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        db::repositories::records::get_repository(f.pool(), "member", "second")
            .await
            .unwrap()
            .is_none(),
        "denied fork creation rolls back its row"
    );
    let (status, _) = browser
        .api("POST", "/api/repos/acme/second/forks", json!({}))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "organization owners retain forking authority"
    );
    request(
        router,
        UPDATE,
        "PATCH",
        path,
        write,
        json!({"members_can_fork_private_repositories":true}),
        200,
    )
    .await;
    let (status, _) = client
        .api("POST", "/api/repos/acme/second/forks", json!({}))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let fork = db::repositories::records::get_repository(f.pool(), "member", "second")
        .await
        .unwrap()
        .unwrap();
    assert!(fork.is_private);
    assert_eq!(fork.forked_from_id, Some(second.id));
    // Prepared Git objects are not an admission: the owning store checks current policy.
    request(
        router,
        UPDATE,
        "PATCH",
        path,
        write,
        json!({"web_commit_signoff_required":false}),
        200,
    )
    .await;
    let store = codeyard::backend::repositories::store::repo_store(second, None);
    let head = store.commit("refs/heads/main").await.unwrap();
    let actor = db::users::accounts::get_user_by_id(f.pool(), owner)
        .await
        .unwrap()
        .unwrap();
    let input = codeyard::backend::repositories::store::FileWrite {
        branch: "main".into(),
        path: "policy.txt".into(),
        message: "Prepared before enabling the policy".into(),
        expected_revision: head.sha.clone(),
        creating: true,
        deleting: false,
    };
    let mut prepared = store
        .prepare_file(
            &input,
            b"Policy content",
            &actor.username,
            &actor.commit_email(),
        )
        .await
        .unwrap();
    prepared.web_commit = true;
    let mut prepared: codeyard::backend::repositories::store::PreparedFile =
        serde_json::from_value(serde_json::to_value(prepared).unwrap()).unwrap();
    request(
        router,
        UPDATE,
        "PATCH",
        path,
        write,
        json!({"web_commit_signoff_required":true}),
        200,
    )
    .await;
    let error = store
        .publish_file_as(f.pool(), second.id, owner, &prepared)
        .await
        .unwrap_err();
    assert_eq!(
        error.kind,
        codeyard::backend::git::error::ErrorKind::Invalid
    );
    assert!(error.message.contains("Signed-off-by"));
    assert_eq!(store.commit("refs/heads/main").await.unwrap().sha, head.sha);
    prepared.branch = "prepared-policy".into();
    prepared.reference = "refs/heads/prepared-policy".into();
    prepared.expected = None;
    prepared.new_branch = true;
    assert!(store
        .publish_file_as(f.pool(), second.id, owner, &prepared)
        .await
        .is_err());
    assert!(store.commit("refs/heads/prepared-policy").await.is_err());
    prepared.web_commit = false;
    store
        .publish_file_as(f.pool(), second.id, owner, &prepared)
        .await
        .unwrap();
    assert_eq!(
        store
            .commit("refs/heads/prepared-policy")
            .await
            .unwrap()
            .parents,
        vec![head.sha]
    );
}

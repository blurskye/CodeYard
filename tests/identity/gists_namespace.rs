//! The new namespace neither hides existing accounts nor lets deleted gist URLs be captured.
use crate::support::database::Database;
use crate::support::http_client::Client;

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_app::gists;
use codeyard_app::gists::CreateGist;
use codeyard_repos_core::repositories::lifecycle;
use codeyard_ci::config::CiConfig;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
use codeyard_db_repos::repositories::creation::RepositoryTarget;
use codeyard_repos::gists::GistCredential;
use codeyard_storage::ObjectStore;
use std::{collections::BTreeMap, sync::Arc};

async fn namespace(pg: bool) {
    let _fixture = super::github_gists::fixture_slot().await;
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let legacy = f.actor("legacy").await;
    let alice = f.actor("alice").await;
    // An installation may already have this account before the name becomes reserved.
    f.execute(
        "UPDATE users SET username='gists',is_admin=TRUE WHERE id=$1",
        &[legacy],
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(AppState::new(
        f.pool().clone(),
        root.path().into(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let router = create_router((*state).clone());
    let a = Client::login(&router, f.pool(), alice).await;
    let owner = Client::login(&router, f.pool(), legacy).await;
    let guest = Client::anonymous(&router);
    let target = RepositoryCreationOwner {
        actor_id: legacy,
        target: RepositoryTarget::Personal(legacy),
    };
    let old_id = "c".repeat(32);
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        target,
        &old_id,
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let private = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        target,
        "new",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let sha = {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        let mut tree = git.treebuilder(None).unwrap();
        tree.insert("a.txt", git.blob(b"legacy bytes\n").unwrap(), 0o100644)
            .unwrap();
        let tree = git.find_tree(tree.write().unwrap()).unwrap();
        let signature = git2::Signature::now("alice", "alice@users.noreply").unwrap();
        git.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Existing data",
            &tree,
            &[],
        )
        .unwrap()
        .to_string()
    };
    assert!(
        codeyard_db_cluster::cluster::bump_write_version(f.pool(), repo.id, None)
            .await
            .unwrap()
    );
    let profile = owner.page_ok("/gists/@users/gists").await;
    let alias = format!("/gists/@repos/gists/{old_id}");
    assert!(profile.contains(&format!("href=\"{alias}\"")));
    assert!(profile.contains("href=\"/gists/@repos/gists/new\""));
    assert_eq!(guest.page("/gists").await.0, 200);
    let redirect = guest
        .exchange(
            "GET",
            &format!("/gists/{old_id}?page=2"),
            "text/html",
            vec![],
        )
        .await;
    assert_eq!(redirect.status, 303);
    assert_eq!(redirect.location, Some(format!("{alias}?page=2")));
    assert!(guest.page_ok(&alias).await.contains("a.txt"));
    let raw = format!("/gists/{old_id}/raw/{sha}/a.txt");
    let redirect = guest.exchange("GET", &raw, "text/plain", vec![]).await;
    assert_eq!(redirect.status, 303);
    assert_eq!(redirect.location, Some(format!("{alias}/raw/{sha}/a.txt")));
    let bytes = guest
        .exchange(
            "GET",
            &format!("{alias}/raw/{sha}/a.txt"),
            "text/plain",
            vec![],
        )
        .await;
    assert_eq!(bytes.status, 200);
    assert_eq!(bytes.body, b"legacy bytes\n");
    // Nested routes must retain the ordinary streamed-upload policy, not the 4 MiB form cap.
    let mut multipart = Vec::new();
    for (name, value) in [
        ("repository_id", repo.id.to_string()),
        ("csrf_token", owner.csrf.clone()),
        ("expected_revision", sha.clone()),
        ("message", "Legacy upload".to_string()),
    ] {
        multipart.extend(format!("--namespace-boundary\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    multipart.extend(b"--namespace-boundary\r\nContent-Disposition: form-data; name=\"files\"; filename=\"large.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n");
    multipart.resize(multipart.len() + 5 * 1024 * 1024, b'x');
    multipart.extend(b"\r\n--namespace-boundary--\r\n");
    let upload = owner
        .exchange(
            "POST",
            &format!("{alias}/upload/main"),
            "multipart/form-data; boundary=namespace-boundary",
            multipart,
        )
        .await;
    assert_eq!(upload.status, 303, "{}", upload.text());
    let uploaded = git2::Repository::open_bare(&repo.disk_path).unwrap();
    let tree = uploaded.head().unwrap().peel_to_tree().unwrap();
    assert_eq!(
        uploaded
            .find_blob(tree.get_name("large.bin").unwrap().id())
            .unwrap()
            .size(),
        5 * 1024 * 1024
    );
    codeyard_db_identity::interactions::set_restriction(
        f.pool(),
        legacy,
        &AccountCredential::Session(&owner.session),
        codeyard_db_identity::interactions::InteractionScope::Repository(repo.id),
        Some(codeyard_db_identity::interactions::NewRestriction {
            limit: codeyard_db_identity::interactions::InteractionGroup::ContributorsOnly,
            expiry: codeyard_db_identity::interactions::InteractionExpiry::OneDay,
        }),
    )
    .await
    .unwrap();
    // The inner route must also prepare contributor proof after its owner/repo parameters match.
    let issue = a
        .form_reply(
            &format!("{alias}/issues/new"),
            &[
                ("title", "Prior contributor"),
                ("body", "An aliased native form"),
                ("intent", "create"),
                ("repository_id", &repo.id.to_string()),
            ],
        )
        .await;
    assert_eq!(issue.status, 303, "{}", issue.text());
    let too_large = owner
        .exchange(
            "POST",
            &format!("{alias}/_forms/about"),
            "application/x-www-form-urlencoded",
            vec![b'x'; 5 * 1024 * 1024],
        )
        .await;
    assert_eq!(too_large.status, 413);
    assert_eq!(owner.page("/gists/@repos/gists/new").await.0, 200);
    assert_ne!(guest.page("/gists/@repos/gists/new").await.0, 200);
    assert_eq!(f.repository(private.id).await.owner_user_id, Some(legacy));
    let follow = a
        .form_reply("/gists/@users/gists/_forms/follow", &[("action", "follow")])
        .await;
    assert_eq!(follow.status, 303, "{}", follow.text());
    assert_eq!(follow.location.as_deref(), Some("/gists/@users/gists"));
    assert!(a.page_ok("/gists/@users/gists").await.contains("Unfollow"));
    assert_eq!(
        codeyard_db_repos::repositories::records::get_git_repository(f.pool(), "gists", &old_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        repo.id
    );

    let staged = lifecycle::provision_gist(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner {
            actor_id: alice,
            target: RepositoryTarget::Personal(alice),
        },
        "gist-staging",
        None,
    )
    .await
    .unwrap();
    let credential = AccountCredential::Session(&a.session);
    let collision = codeyard_db_repos::gists::reserve(
        f.pool(),
        alice,
        &credential,
        &codeyard_db_repos::gists::NewGist {
            repo_id: staged.id,
            id: &old_id,
            description: "Collision",
            is_public: false,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        collision.downcast_ref::<codeyard_db_repos::gists::GistRefusal>(),
        Some(codeyard_db_repos::gists::GistRefusal::Conflict)
    ));
    assert!(!codeyard_db_repos::gists::is_identifier(f.pool(), &old_id)
        .await
        .unwrap());
    let gist = gists::create(
        state.clone(),
        alice,
        GistCredential::Session(a.session.clone()),
        CreateGist {
            description: "Deleted".into(),
            is_public: false,
            files: BTreeMap::from([("a.txt".into(), "original".into())]),
        },
    )
    .await
    .unwrap();
    let id = gist.gist.id;
    gists::delete(
        state.clone(),
        alice,
        GistCredential::Session(a.session.clone()),
        id.clone(),
    )
    .await
    .unwrap();
    assert!(codeyard_db_repos::gists::is_identifier(f.pool(), &id)
        .await
        .unwrap());
    assert!(
        codeyard_db_repos::repositories::records::get_git_repository(f.pool(), "gists", &id)
            .await
            .unwrap()
            .is_none()
    );
    let reused = codeyard_db_repos::gists::reserve(
        f.pool(),
        alice,
        &credential,
        &codeyard_db_repos::gists::NewGist {
            repo_id: staged.id,
            id: &id,
            description: "Reuse",
            is_public: false,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        reused.downcast_ref::<codeyard_db_repos::gists::GistRefusal>(),
        Some(codeyard_db_repos::gists::GistRefusal::Conflict)
    ));
    let count = f.count().await;
    assert!(lifecycle::provision(
        (f.pool(), None),
        root.path(),
        target,
        &id,
        None,
        false,
        None
    )
    .await
    .is_err());
    assert_eq!(f.count().await, count);
    let renamed =
        codeyard_db_repos::repositories::records::rename_repository(f.pool(), legacy, repo.id, &id)
            .await
            .unwrap_err();
    assert!(matches!(
        renamed
            .downcast_ref::<codeyard_db_repos::repositories::records::RepositorySettingsRefusal>(),
        Some(codeyard_db_repos::repositories::records::RepositorySettingsRefusal::NameConflict)
    ));
    let ordinary = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner {
            actor_id: alice,
            target: RepositoryTarget::Personal(alice),
        },
        &id,
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let moved = codeyard_db_repos::repositories::records::transfer_repository(
        f.pool(),
        legacy,
        ordinary.id,
        RepositoryTarget::Personal(legacy),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        moved.downcast_ref::<codeyard_db_repos::repositories::records::RepositorySettingsRefusal>(),
        Some(codeyard_db_repos::repositories::records::RepositorySettingsRefusal::NameConflict)
    ));
    assert_eq!(f.repository(ordinary.id).await.owner_user_id, Some(alice));
    // A legacy/imported row that bypassed admission still cannot capture an issued URL.
    f.execute(
        &format!("UPDATE repositories SET name='{id}' WHERE id=$1"),
        &[repo.id],
    )
    .await;
    assert!(
        codeyard_db_repos::repositories::records::get_git_repository(f.pool(), "gists", &id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(guest.page(&format!("/gists/{id}")).await.0, 404);
    assert_eq!(
        guest
            .exchange(
                "GET",
                &format!("/gists/{id}/raw/{sha}/a.txt"),
                "text/plain",
                vec![]
            )
            .await
            .status,
        404
    );
    assert_eq!(
        guest.page(&format!("/gists/@repos/gists/{id}")).await.0,
        200
    );
    assert!(
        codeyard_db_repos::gists::discard(f.pool(), staged.id, false)
            .await
            .unwrap()
            .is_some()
    );
    // The same namespace protection applies to pre-existing organizations, not just users.
    f.execute(
        "UPDATE users SET username='former-gists-user' WHERE id=$1",
        &[legacy],
    )
    .await;
    let input = codeyard_db_orgs::organizations::profiles::NewOrganization::new(
        "legacy-org",
        codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None).unwrap(),
    )
    .unwrap();
    let org = match codeyard_db_orgs::organizations::profiles::create_organization(
        f.pool(),
        alice,
        &input,
    )
    .await
    .unwrap()
    {
        codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(org) => org,
        _ => panic!("organization refused"),
    };
    f.execute(
        "UPDATE organizations SET name='gists' WHERE id=$1",
        &[org.id],
    )
    .await;
    let org_target = RepositoryCreationOwner {
        actor_id: alice,
        target: RepositoryTarget::Organization(org.id),
    };
    lifecycle::provision(
        (f.pool(), None),
        root.path(),
        org_target,
        &old_id,
        None,
        false,
        None,
    )
    .await
    .unwrap();
    assert!(guest
        .page_ok("/gists/@users/gists")
        .await
        .contains(&format!("/gists/@repos/gists/{old_id}")));
    assert!(lifecycle::provision(
        (f.pool(), None),
        root.path(),
        org_target,
        &id,
        None,
        false,
        None
    )
    .await
    .is_err());
    let (_, document) = super::github::request(
        &router,
        "orgs/get",
        "GET",
        "/api/v3/orgs/gists",
        None,
        serde_json::Value::Null,
        200,
    )
    .await;
    assert!(document["html_url"]
        .as_str()
        .unwrap()
        .ends_with("/gists/@users/gists"));
    let org = codeyard_db_orgs::organizations::profiles::get_public_organization(f.pool(), "gists")
        .await
        .unwrap()
        .unwrap();
    assert!(
        codeyard_webhooks::payloads::team_organization(&org, 1, true)["html_url"]
            .as_str()
            .unwrap()
            .ends_with("/gists/@users/gists")
    );
    f.finish().await;
}
#[tokio::test]
async fn sqlite_gists_namespace() {
    namespace(false).await
}
#[tokio::test]
async fn postgres_gists_namespace() {
    namespace(true).await
}

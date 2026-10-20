//! Profile activity lists what a user pushed, opened and commented on, and hides
//! items in repositories the viewer cannot read.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;
use crate::support::pull_requests as pulls;

use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, identity::activity, storage::ObjectStore,
};
use database::Database;
use pulls::Client;
use serde_json::Value;
use std::sync::Arc;

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.execute(
        "INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE",
        &[],
    )
    .await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let pool = f.pool().clone();
    let public = db::repositories::records::create_repository(
        &pool,
        alice,
        "pub",
        None,
        false,
        "/owned/activity.git",
    )
    .await
    .unwrap();
    let secret = db::repositories::records::create_repository(
        &pool,
        alice,
        "secret",
        None,
        true,
        "/owned/activity-secret.git",
    )
    .await
    .unwrap();
    // alice creates a repository for an organization: it is listed under alice.
    let acme = match db::organizations::create_organization(
        &pool,
        alice,
        &db::organizations::NewOrganization::new(
            "acme",
            db::organizations::OrganizationProfile::new(None, None).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    {
        db::organizations::OrganizationOutcome::Applied(metadata) => metadata,
        _ => panic!("the owner creates the organization"),
    };
    db::repositories::creation::insert_repository(
        &pool,
        db::repositories::creation::NewRepository {
            owner: db::repositories::creation::RepositoryCreationOwner {
                actor_id: alice,
                target: db::repositories::creation::RepositoryTarget::Organization(acme.id),
            },
            name: "tools",
            description: None,
            is_private: false,
            disk_path: "/owned/activity-org.git",
            storage_node_id: None,
            source_id: None,
        },
    )
    .await
    .unwrap()
    .expect("organization repository is created");
    db::activity::record_repository_push(&pool, public.id, alice, "main", 3)
        .await
        .unwrap();
    db::activity::record_repository_push(&pool, secret.id, alice, "main", 1)
        .await
        .unwrap();
    db::issues::create_issue(&pool, secret.id, alice, "hidden issue", "")
        .await
        .unwrap();
    // Zero-commit branch updates must not consume visible activity slots.
    for _ in 0..30 {
        db::activity::record_repository_push(&pool, public.id, alice, "existing", 0)
            .await
            .unwrap();
    }
    // Thirty-two public issues, so the list needs a second page.
    for number in 0..32 {
        db::issues::create_issue(&pool, public.id, alice, &format!("issue {number}"), "")
            .await
            .unwrap();
    }

    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        pool.clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let anonymous = Client::anonymous(&router);
    let outsider = Client::login(&router, &pool, bob).await;
    let owner = Client::login(&router, &pool, alice).await;

    // Toggling a follow does not create another notice within the durable cooldown.
    for _ in 0..3 {
        outsider
            .api("PUT", "/api/users/alice/follow", Value::Null)
            .await;
        outsider
            .api("DELETE", "/api/users/alice/follow", Value::Null)
            .await;
    }
    let notices = db::notifications::records::list_notifications_for_user(&pool, alice, 100, false)
        .await
        .unwrap();
    assert_eq!(
        notices
            .iter()
            .filter(|notice| notice.kind == "user_followed")
            .count(),
        1
    );

    // Visible to others: the public push, the public repository, and 32 issues.
    let (status, first) = anonymous
        .api("GET", "/api/users/alice/activity?page=1", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let items = first["items"].as_array().unwrap();
    assert_eq!(items.len(), 30);
    assert_eq!(first["next_page"], 2);
    let (status, second) = anonymous
        .api("GET", "/api/users/alice/activity?page=2", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    let rest = second["items"].as_array().unwrap();
    assert_eq!(rest.len(), 5);
    assert_eq!(second["next_page"], Value::Null);
    let everything: Vec<&Value> = items.iter().chain(rest.iter()).collect();
    assert!(everything
        .iter()
        .all(|item| item["repository"] == "alice/pub" || item["repository"] == "acme/tools"));
    assert!(everything
        .iter()
        .any(|item| item["kind"] == "repository_created" && item["repository"] == "acme/tools"));
    assert!(everything.iter().any(|item| item["kind"] == "push"
        && item["branch"] == "main"
        && item["commits"] == 3
        && item["url"] == "/alice/pub/commits?ref=main"));
    assert!(everything
        .iter()
        .any(|item| item["kind"] == "repository_created"));
    assert!(everything.iter().any(|item| item["kind"] == "issue_opened"
        && item["url"]
            .as_str()
            .unwrap()
            .starts_with("/alice/pub/issues/")));

    assert!(everything.iter().all(|item| item["branch"] != "existing"));
    // A signed-in user without access sees the same list.
    let (status, value) = outsider
        .api("GET", "/api/users/alice/activity?page=1", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["items"].as_array().unwrap().len(), 30);

    // The owner sees the private repository's items as well.
    let (status, value) = owner
        .api("GET", "/api/users/alice/activity?page=2", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    let owner_items = value["items"].as_array().unwrap();
    assert!(owner_items
        .iter()
        .any(|item| item["repository"] == "alice/secret"));

    let raw = db::activity::list_user_activity(&pool, alice, None, 100, 0)
        .await
        .unwrap();
    assert!(
        raw.iter().all(|row| row.repo_id != secret.id),
        "SQL authorizes each scan together with its content"
    );
    f.execute(
        "UPDATE repositories SET is_private=TRUE WHERE id=$1",
        &[public.id],
    )
    .await;
    f.execute(
        "UPDATE issues SET title='new private title' WHERE repo_id=$1",
        &[public.id],
    )
    .await;
    let later = db::activity::list_user_activity(&pool, alice, None, 100, 0)
        .await
        .unwrap();
    assert!(later.iter().all(|row| row.repo_id != public.id));
    f.finish().await;
}

async fn repository_pages(pg: bool) {
    let mut f = Database::new(pg).await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    for index in 0..65 {
        db::repositories::records::create_repository(
            f.pool(),
            alice,
            &format!("repo-{index:03}"),
            None,
            index < 30,
            "/fixture/unread.git",
        )
        .await
        .unwrap();
    }
    let (_, first, count) = codeyard::backend::identity::profiles::repositories_on_profile(
        f.pool(),
        "alice",
        Some(bob),
        "",
        1,
    )
    .await
    .unwrap();
    assert_eq!(count, 35);
    assert_eq!(first.len(), 30);
    assert!(first.iter().all(|repo| !repo.is_private));
    let (_, second, _) = codeyard::backend::identity::profiles::repositories_on_profile(
        f.pool(),
        "alice",
        Some(bob),
        "",
        2,
    )
    .await
    .unwrap();
    assert_eq!(second.len(), 5);
    assert!(first
        .iter()
        .all(|repo| second.iter().all(|other| repo.id != other.id)));
    let (_, own, own_count) = codeyard::backend::identity::profiles::repositories_on_profile(
        f.pool(),
        "alice",
        Some(alice),
        "",
        1,
    )
    .await
    .unwrap();
    assert_eq!(own_count, 65);
    assert_eq!(own.len(), 30);
    assert!(own.iter().all(|repo| repo.is_private));
    f.finish().await;
}

#[tokio::test]
async fn sqlite_profile_repository_pages() {
    repository_pages(false).await;
}
#[tokio::test]
async fn postgres_profile_repository_pages() {
    repository_pages(true).await;
}

#[tokio::test]
async fn sqlite_profile_activity_pages_and_hides_private_repositories() {
    contract(false).await;
}

/// One request reads at most `MAX_SCAN_ROWS` rows. A thousand private issues are newer than the
/// older public issue, so the scan stops before it. Only the newest public issue is listed.
async fn scan_stops_at_the_row_bound(pg: bool) {
    let mut f = Database::new(pg).await;
    f.execute(
        "INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE",
        &[],
    )
    .await;
    let alice = f.actor("alice").await;
    let pool = f.pool().clone();
    let public = db::repositories::records::create_repository(
        &pool,
        alice,
        "pub",
        None,
        false,
        "/owned/scan.git",
    )
    .await
    .unwrap();
    let secret = db::repositories::records::create_repository(
        &pool,
        alice,
        "secret",
        None,
        true,
        "/owned/scan-secret.git",
    )
    .await
    .unwrap();
    db::issues::create_issue(&pool, public.id, alice, "older public", "")
        .await
        .unwrap();
    for number in 0..activity::MAX_SCAN_ROWS {
        db::issues::create_issue(&pool, secret.id, alice, &format!("private {number}"), "")
            .await
            .unwrap();
    }
    db::issues::create_issue(&pool, public.id, alice, "newest public", "")
        .await
        .unwrap();

    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        pool.clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let anonymous = Client::anonymous(&router);
    let (status, page) = anonymous
        .api("GET", "/api/users/alice/activity?page=1", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{page}");
    assert!(items[0]["url"]
        .as_str()
        .unwrap()
        .starts_with("/alice/pub/issues/"));
    assert_eq!(page["next_page"], Value::Null);
    f.finish().await;
}

#[tokio::test]
async fn postgres_profile_activity_pages_and_hides_private_repositories() {
    contract(true).await;
}

#[tokio::test]
async fn sqlite_profile_activity_scan_stops_at_the_row_bound() {
    scan_stops_at_the_row_bound(false).await;
}

#[tokio::test]
async fn postgres_profile_activity_scan_stops_at_the_row_bound() {
    scan_stops_at_the_row_bound(true).await;
}

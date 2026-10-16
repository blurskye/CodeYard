//! Profile activity lists what a user pushed, opened and commented on, and hides
//! items in repositories the viewer cannot read.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

#[path = "support/database.rs"]
mod database;
#[path = "editor_save/fixture.rs"]
mod fixture;
#[path = "support/native_database.rs"]
mod native;
#[path = "support/production_ci.rs"]
mod production;
#[path = "support/pull_requests.rs"]
mod pulls;

use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
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

    f.finish().await;
}

#[tokio::test]
async fn sqlite_profile_activity_pages_and_hides_private_repositories() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_profile_activity_pages_and_hides_private_repositories() {
    contract(true).await;
}

//! Deploy keys through the JSON API and the settings forms, on SQLite and PostgreSQL: only repository
//! admins manage them, a key is unique across account keys and deploy keys, a deleted key stops
//! being listed, and the settings section renders for admins only.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests::Client;
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    crypto::SecretEncryptor,
    db::{self, users::ssh_keys},
    storage::ObjectStore,
};
use database::Database;
use serde_json::json;
use std::sync::Arc;

// Two valid public keys from the account-key tests.
const ED25519: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDYe6ruRitXiLJXGkQcJeqR3EVAP9w4+lUEYv6qEVxnoxKP+27ZP59uz3sCO1kstXuPIcyMLCH/uRBbu05ix/1OxYQjIhsz9S9NeoNy3zuMTfvLxV+3G7nOsFoXY2V5nqKHxcFeau3LVv4mf9ZDvO9lzEncNu6rvTGRIMaFAdiApVNWt+E+fJQ53pVL401NG/xsH6zdW3muV2ZAOpQJsVJi9ac4p4BCR3xOk4MLvVWw01Cf1IK46duTXhBZ+A+dN5KFSnJprDtKZYpexh1NNPVrE4OvK9WKgR/GHW9O5MxdGpfT2GeSd0XD/adrsRnIQprHqwV3C5Y5vur12yX1VyYD";
const ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBNulZrf4sHZbqSp2mLJce/VC3rrYC0TuAptZIv0wy1ZZJjvdhkPl+5hakMGej2UxMvk5FVnh+OUOBBtM5N4FFnY=";

async fn setup(pg: bool) -> (Database, tempfile::TempDir, Client, Client) {
    bundle::install();
    let database = Database::new(pg).await;
    database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = database.actor("alice").await;
    let bob_id = database.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    codeyard::backend::repositories::lifecycle::provision(
        (database.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = create_router(AppState::new(
        database.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(Arc::new(SecretEncryptor::new(&[42; 32]))),
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, database.pool(), alice_id).await;
    let bob = Client::login(&router, database.pool(), bob_id).await;
    (database, root, alice, bob)
}

async fn repo_id(database: &Database) -> i64 {
    db::repositories::records::get_repository(database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap()
        .id
}

async fn keys_api_and_forms(pg: bool) {
    let (database, _root, alice, bob) = setup(pg).await;
    let repo = repo_id(&database).await;
    let rid = repo.to_string();
    let path = "/api/repos/alice/proj/keys";

    // Only admins manage keys: a reader is refused on every route.
    let (status, _) = bob
        .api(
            "POST",
            path,
            json!({"title": "ci", "key": ED25519, "can_write": false}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = bob.api("GET", path, json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = bob
        .form(
            "/alice/proj/_forms/deploy-key-add",
            &[
                ("repository_id", rid.as_str()),
                ("title", "ci"),
                ("key", ED25519),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An invalid key and an empty title are refused.
    let (status, _) = alice
        .api(
            "POST",
            path,
            json!({"title": "ci", "key": "ssh-ed25519 AAAA", "can_write": false}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = alice
        .api("POST", path, json!({"title": " ", "key": ED25519}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // An admin adds a read-only key, and sees it listed.
    let (status, created) = alice
        .api(
            "POST",
            path,
            json!({"title": "ci", "key": ED25519, "can_write": false}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["can_write"], false);
    let key_id = created["id"].as_i64().unwrap();
    let (status, listed) = alice.api("GET", path, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["title"], "ci");

    // The same key is refused again, as a deploy key or as an account key.
    let (status, text) = alice
        .api("POST", path, json!({"title": "again", "key": ED25519}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    let owner = db::users::accounts::get_user_by_username(database.pool(), "alice")
        .await
        .unwrap()
        .unwrap();
    ssh_keys::add_ssh_key(database.pool(), owner.id, "laptop", ECDSA)
        .await
        .unwrap();
    let (status, _) = alice
        .api("POST", path, json!({"title": "dup", "key": ECDSA}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let refused = ssh_keys::add_ssh_key(database.pool(), owner.id, "dup", ED25519)
        .await
        .unwrap_err();
    assert_eq!(
        refused.downcast_ref::<ssh_keys::SshKeyWriteRefusal>(),
        Some(&ssh_keys::SshKeyWriteRefusal::AlreadyUsed)
    );

    // The settings form adds and deletes the same way, and redirects with a notice.
    let (status, _) = alice
        .form(
            "/alice/proj/_forms/deploy-key-add",
            &[
                ("repository_id", rid.as_str()),
                ("title", "build"),
                ("key", "ssh-ed25519 AAAA"),
                ("can_write", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let reply = alice
        .form_reply(
            "/alice/proj/_forms/deploy-key-delete",
            &[
                ("repository_id", rid.as_str()),
                ("key_id", &key_id.to_string()),
                ("confirm", "yes"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let rows = db::deploy_keys::list_deploy_keys(database.pool(), repo)
        .await
        .unwrap();
    assert!(rows.is_empty(), "the form deleted the key");

    // An unknown key is not found, and a listed key is deleted.
    let (status, created) = alice
        .api(
            "POST",
            path,
            json!({"title": "write", "key": RSA, "can_write": true}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let write_id = created["id"].as_i64().unwrap();
    let (status, _) = alice
        .api("DELETE", &format!("{path}/{}", write_id + 999), json!({}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = alice
        .api("DELETE", &format!("{path}/{write_id}"), json!({}))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, listed) = alice.api("GET", path, json!({})).await;
    assert!(listed.as_array().unwrap().is_empty());

    // The section renders for admins and is refused to readers.
    alice
        .form(
            "/alice/proj/_forms/deploy-key-add",
            &[
                ("repository_id", rid.as_str()),
                ("title", "ci"),
                ("key", ED25519),
            ],
        )
        .await;
    let (status, page) = alice.page("/alice/proj/settings?tab=deploy-keys").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Deploy keys"), "{page}");
    assert!(page.contains("SHA256:"), "{page}");
    let (status, _) = bob.page("/alice/proj/settings?tab=deploy-keys").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn sqlite_deploy_keys_follow_repository_admin_rules() {
    keys_api_and_forms(false).await;
}

#[tokio::test]
async fn postgres_deploy_keys_follow_repository_admin_rules() {
    keys_api_and_forms(true).await;
}

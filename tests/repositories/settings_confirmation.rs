//! The general settings form: saving only a description needs no confirmation, while changing
//! visibility is refused without the explicit confirmation. On SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests::Client;

use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
};
use database::Database;
use serde_json::Value;
use std::sync::Arc;

async fn contract(pg: bool) {
    // The native form answers with a rendered page, which needs the browser bundle.
    bundle::install();
    let f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
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
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let proj = alice
        .expect("GET", "/api/repos/alice/proj", Value::Null, StatusCode::OK)
        .await;
    let repo = proj["id"].as_i64().unwrap().to_string();
    let settings = "/alice/proj/_forms/settings";

    // Only the description changes, so no confirmation is asked for.
    let reply = alice
        .form_reply(
            settings,
            &[
                ("repository_id", repo.as_str()),
                ("description", "Edited"),
                ("visibility", "public"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let saved = alice
        .expect("GET", "/api/repos/alice/proj", Value::Null, StatusCode::OK)
        .await;
    assert_eq!(saved["description"], "Edited", "{saved}");
    assert_eq!(saved["is_private"], false, "{saved}");

    // Going private without the confirmation is refused, and nothing changes.
    let (status, page) = alice
        .form(
            settings,
            &[
                ("repository_id", repo.as_str()),
                ("description", "Edited"),
                ("visibility", "private"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert!(
        page.contains("Explicit confirmation is required."),
        "{page}"
    );
    let kept = alice
        .expect("GET", "/api/repos/alice/proj", Value::Null, StatusCode::OK)
        .await;
    assert_eq!(kept["is_private"], false, "{kept}");

    // With the confirmation the visibility changes.
    let reply = alice
        .form_reply(
            settings,
            &[
                ("repository_id", repo.as_str()),
                ("description", "Edited"),
                ("visibility", "private"),
                ("confirm", "yes"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let private = alice
        .expect("GET", "/api/repos/alice/proj", Value::Null, StatusCode::OK)
        .await;
    assert_eq!(private["is_private"], true, "{private}");

    // An unconfirmed save based on the earlier public row must not reopen it.
    let refused = db::repositories::records::update_repository(
        f.pool(),
        alice_id,
        repo.parse().unwrap(),
        Some("stale description"),
        db::repositories::records::VisibilityUpdate {
            is_private: false,
            expected: Some(false),
        },
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None,
        },
    )
    .await;
    assert!(refused.is_err());
    let kept = db::repositories::records::get_repository_by_id(f.pool(), repo.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(kept.is_private);
    assert_eq!(kept.description.as_deref(), Some("Edited"));

    // A description-only save of a private repository needs no confirmation either.
    let reply = alice
        .form_reply(
            settings,
            &[
                ("repository_id", repo.as_str()),
                ("description", "Still private"),
                ("visibility", "private"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
}

#[tokio::test]
async fn sqlite_description_only_settings_save_needs_no_visibility_confirmation() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_description_only_settings_save_needs_no_visibility_confirmation() {
    contract(true).await;
}

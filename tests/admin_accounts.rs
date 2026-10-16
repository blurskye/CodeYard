//! The administration account list reads recorded repository sizes. A GET never
//! scans repositories, so an uncached or missing repository cannot slow it down
//! or make it fail, on SQLite and PostgreSQL.
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
use codeyard::backend::{api::create_router, app::AppState, db, storage::ObjectStore};
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
    let admin_id = f.actor("admin").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[admin_id])
        .await;
    let owner_id = f.actor("owner").await;
    let root = tempfile::tempdir().unwrap();
    // The bare repository has bytes on disk, but no size has been recorded for it.
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(repository.size_bytes, 0);
    // A repository whose files are missing must not fail the list either.
    db::repositories::records::create_repository(
        f.pool(),
        owner_id,
        "gone",
        None,
        false,
        root.path().join("gone.git").to_str().unwrap(),
    )
    .await
    .unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        codeyard::backend::ci::CiConfig::default(),
        None,
    ));
    let admin = Client::login(&router, f.pool(), admin_id).await;
    let (status, accounts) = admin
        .api("GET", "/api/auth/admin/accounts", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{accounts}");
    let owner = accounts
        .as_array()
        .unwrap()
        .iter()
        .find(|account| account["username"] == "owner")
        .unwrap();
    assert_eq!(owner["total_bytes"], 0);
    // The GET did not scan the repository or record its size.
    let stored = db::repositories::records::get_repository_by_id(f.pool(), repository.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.size_bytes, 0);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_admin_account_list_reads_recorded_sizes_only() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_admin_account_list_reads_recorded_sizes_only() {
    contract(true).await;
}

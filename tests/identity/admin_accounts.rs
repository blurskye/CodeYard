//! The administration account list reads recorded repository sizes. A GET never
//! scans repositories, so an uncached or missing repository cannot slow it down
//! or make it fail, on SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;
use crate::support::http_client as pulls;

use axum::http::StatusCode;
use codeyard::backend::db;
use database::Database;
use pulls::Client;
use serde_json::Value;

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
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
    let router = f.router(root.path()).await;
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
    assert_eq!(owner["commits_total"], 0);
    db::activity::record_repository_push(f.pool(), repository.id, owner_id, "main", 5)
        .await
        .unwrap();
    let (_, recorded) = admin
        .api("GET", "/api/auth/admin/accounts", Value::Null)
        .await;
    let recorded = recorded
        .as_array()
        .unwrap()
        .iter()
        .find(|account| account["username"] == "owner")
        .unwrap();
    assert_eq!(recorded["commits_total"], 5);
    assert_eq!(
        recorded["pushes_total"], 0,
        "the documented push counter counts CI runs"
    );
    let chart = db::users::accounts::list_admin_activity_points(f.pool(), 7)
        .await
        .unwrap();
    assert!(chart.last().unwrap().commits >= 5);
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

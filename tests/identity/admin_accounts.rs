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

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
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
    let (status, page) = admin.model("/admin").await;
    assert_eq!(status, StatusCode::OK);
    let codeyard::webui::app::Page::Identity(codeyard::webui::pages::account::Page::Admin {
        model,
    }) = page
    else {
        panic!("administration overview")
    };
    assert!(model.storage_bytes.iter().all(|bytes| *bytes == 0));
    assert!(model.activity.iter().all(|point| point.commits == 0));
    let accounts = db::users::accounts::list_admin_account_summaries(f.pool(), 100, 0)
        .await
        .unwrap();
    let owner = accounts
        .iter()
        .find(|account| account.username == "owner")
        .unwrap();
    assert_eq!(owner.total_bytes, 0);
    assert_eq!(owner.commits_total, 0);
    db::activity::record_repository_push(f.pool(), repository.id, owner_id, "main", 5)
        .await
        .unwrap();
    let (status, page) = admin.model("/admin").await;
    assert_eq!(status, StatusCode::OK);
    let codeyard::webui::app::Page::Identity(codeyard::webui::pages::account::Page::Admin {
        model,
    }) = page
    else {
        panic!("administration overview")
    };
    assert!(model.storage_bytes.iter().all(|bytes| *bytes == 0));
    assert_eq!(
        model
            .activity
            .iter()
            .map(|point| point.commits)
            .sum::<i64>(),
        5
    );
    let recorded = db::users::accounts::list_admin_account_summaries(f.pool(), 100, 0)
        .await
        .unwrap();
    let recorded = recorded
        .iter()
        .find(|account| account.username == "owner")
        .unwrap();
    assert_eq!(recorded.commits_total, 5);
    assert_eq!(recorded.pushes_total, 0, "the push counter counts CI runs");
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
    let (status, page) = admin.model("/admin/email").await;
    assert_eq!(status, StatusCode::OK);
    assert!(matches!(page, codeyard::webui::app::Page::Email(_)));
    let invalid = admin
        .form_reply(
            "/admin/email",
            &[("host", "bad host"), ("from_address", "invalid")],
        )
        .await;
    assert_eq!(
        invalid.status,
        StatusCode::BAD_REQUEST,
        "{}",
        invalid.text()
    );
    // The settings are read first, then the administrator and their address at send time.
    let test = admin.form_reply("/admin/email/test", &[]).await;
    assert_eq!(test.status, StatusCode::BAD_GATEWAY, "{}", test.text());
    assert!(
        test.text().contains("Email is not set up"),
        "{}",
        test.text()
    );
    let saved = admin
        .form_reply(
            "/admin/email",
            &[
                ("host", "smtp.example.test"),
                ("from_address", "forge@example.test"),
            ],
        )
        .await;
    assert_eq!(
        saved.location.as_deref(),
        Some("/admin/email?saved=1"),
        "{}",
        saved.text()
    );
    let test = admin.form_reply("/admin/email/test", &[]).await;
    assert_eq!(test.status, StatusCode::BAD_REQUEST, "{}", test.text());
    assert!(
        test.text().contains("Your account has no email address"),
        "{}",
        test.text()
    );
    let member = Client::login(&router, f.pool(), owner_id).await;
    for path in ["/admin/email", "/admin/email/test"] {
        let reply = member.form_reply(path, &[]).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text());
    }
    for (method, path) in [
        ("POST", "/api/auth/admin/users".to_owned()),
        ("GET", "/api/auth/admin/accounts".to_owned()),
        ("PATCH", format!("/api/auth/admin/accounts/{owner_id}/ban")),
        ("GET", "/api/auth/admin/signup-requests".to_owned()),
        ("POST", "/api/auth/admin/signup-requests/resolve".to_owned()),
        ("GET", "/api/auth/admin/ai-crawlers".to_owned()),
        ("PUT", "/api/auth/admin/ai-crawlers".to_owned()),
        ("GET", "/api/auth/admin/email".to_owned()),
        ("PUT", "/api/auth/admin/email".to_owned()),
        ("POST", "/api/auth/admin/email/test".to_owned()),
    ] {
        let reply = admin
            .exchange(method, &path, "application/json", b"{}".to_vec())
            .await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{method} {path}: {}",
            reply.text()
        );
    }
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

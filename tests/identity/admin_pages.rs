//! The administration forms: ban, role, signup request decisions, account creation and the
//! refusal of everyone who is not an administrator, on SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use crate::support::http_client::Client;

use axum::http::StatusCode;
use codeyard::backend::{db, identity::passwords};
use codeyard::webui::{app::Page, pages::account};
use db::users::signup_requests as requests;

async fn user(f: &Database, name: &str) -> db::models::User {
    db::users::accounts::get_user_by_username(f.pool(), name)
        .await
        .unwrap()
        .unwrap()
}

async fn admin_forms(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    db::node_config::upsert_node_configuration(f.pool(), "private", true)
        .await
        .unwrap();
    let admin = f.actor("forms_admin").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[admin])
        .await;
    let member = f.actor("forms_member").await;
    let target = f.actor("forms_target").await;
    let hash = passwords::hash_password("sufficient-password")
        .await
        .unwrap();
    let accept = requests::create_signup_request(f.pool(), "forms_accept", None, &hash)
        .await
        .unwrap();
    let decline = requests::create_signup_request(f.pool(), "forms_decline", None, &hash)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let client = Client::login(&router, f.pool(), admin).await;
    let outsider = Client::login(&router, f.pool(), member).await;
    let ban = format!("/admin/accounts/{target}/ban");
    let role = format!("/admin/accounts/{target}/role");

    // A member cannot open the pages or send any of the forms.
    for path in [
        "/admin",
        "/admin/users",
        "/admin/users/new",
        "/admin/settings",
    ] {
        let (status, _) = outsider.model(path).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }
    for (path, pairs) in [
        (
            ban.as_str(),
            vec![("banned", "true"), ("confirm_delete", "yes")],
        ),
        (role.as_str(), vec![("admin", "true")]),
        (
            "/admin/users",
            vec![
                ("username", "x_outsider"),
                ("password", "sufficient-password"),
            ],
        ),
    ] {
        let reply = outsider.form_reply(path, &pairs).await;
        assert!(
            !reply.status.is_success() && reply.location.is_none(),
            "{path}"
        );
    }
    let reply = outsider
        .form_reply(
            &format!("/admin/users/requests/{}/resolve", accept.id),
            &[("action", "approve")],
        )
        .await;
    assert!(!reply.status.is_success() && reply.location.is_none());
    assert_eq!(
        requests::count_signup_requests(f.pool(), "pending")
            .await
            .unwrap(),
        2
    );

    // Banning needs the confirmation field; unbanning does not.
    let reply = client.form_reply(&ban, &[("banned", "true")]).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(!user(&f, "forms_target").await.is_banned);
    let reply = client
        .form_reply(&ban, &[("banned", "true"), ("confirm_delete", "yes")])
        .await;
    assert_eq!(reply.location.as_deref(), Some("/admin/users?saved=ban"));
    assert!(user(&f, "forms_target").await.is_banned);
    let reply = client.form_reply(&ban, &[("banned", "false")]).await;
    assert_eq!(reply.location.as_deref(), Some("/admin/users?saved=ban"));
    assert!(!user(&f, "forms_target").await.is_banned);

    // The administrator role is granted and removed.
    let reply = client.form_reply(&role, &[("admin", "true")]).await;
    assert_eq!(reply.location.as_deref(), Some("/admin/users?saved=role"));
    assert!(user(&f, "forms_target").await.is_admin);
    let reply = client.form_reply(&role, &[("admin", "false")]).await;
    assert_eq!(reply.location.as_deref(), Some("/admin/users?saved=role"));
    assert!(!user(&f, "forms_target").await.is_admin);

    // Signup requests are listed on the users page and decided there.
    let (status, page) = client.model("/admin/users").await;
    assert_eq!(status, StatusCode::OK);
    let codeyard::webui::app::Page::Identity(codeyard::webui::pages::account::Page::AdminUsers {
        model,
        ..
    }) = page
    else {
        panic!("users page")
    };
    assert_eq!((model.requests.len(), model.requests_count), (2, 2));
    let reply = client
        .form_reply(
            &format!("/admin/users/requests/{}/resolve", accept.id),
            &[("action", "approve")],
        )
        .await;
    assert_eq!(
        reply.location.as_deref(),
        Some("/admin/users?saved=request")
    );
    user(&f, "forms_accept").await;
    let reply = client
        .form_reply(
            &format!("/admin/users/requests/{}/resolve", decline.id),
            &[("action", "reject")],
        )
        .await;
    assert_eq!(
        reply.location.as_deref(),
        Some("/admin/users?saved=request")
    );
    assert!(
        db::users::accounts::get_user_by_username(f.pool(), "forms_decline")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        requests::count_signup_requests(f.pool(), "pending")
            .await
            .unwrap(),
        0
    );

    // A refused account comes back to the new-user form with the name kept.
    let (status, page) = client.model("/admin/users/new").await;
    assert_eq!(status, StatusCode::OK);
    assert!(matches!(
        page,
        Page::Identity(account::Page::AdminNewUser { .. })
    ));
    let reply = client
        .form_reply(
            "/admin/users",
            &[("username", "forms_short"), ("password", "short")],
        )
        .await;
    assert!(!reply.status.is_success() && reply.location.is_none());
    let body = String::from_utf8(reply.body).unwrap();
    assert!(body.contains("value=\"forms_short\""), "{body}");
    // The refused form comes back as the New user page, not the account list.
    assert!(
        body.contains("New user") && !body.contains("Search accounts"),
        "{body}"
    );

    // An administrator creates an account with a temporary password.
    let reply = client
        .form_reply(
            "/admin/users",
            &[
                ("username", "forms_created"),
                ("password", "sufficient-password"),
                ("is_admin", "on"),
            ],
        )
        .await;
    assert_eq!(
        reply.location.as_deref(),
        Some("/admin/users?saved=account")
    );
    let created = user(&f, "forms_created").await;
    assert!(created.must_change_password && created.is_admin);

    // The audit log names the accounts, and the filter keeps to one kind of change.
    let (status, page) = client.model("/admin/audit?action=user.ban").await;
    assert_eq!(status, StatusCode::OK);
    let Page::Identity(account::Page::AdminAudit { model }) = page else {
        panic!("audit model")
    };
    assert!(!model.entries.is_empty());
    assert!(model.entries.iter().all(|entry| entry.action == "user.ban"
        && entry.actor == "forms_admin"
        && entry.target == "forms_target"));
    let (status, page) = client.model("/admin/audit?action=user.create").await;
    assert_eq!(status, StatusCode::OK);
    let Page::Identity(account::Page::AdminAudit { model }) = page else {
        panic!("audit model")
    };
    assert_eq!(model.entries[0].target, "forms_created");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_admin_forms_change_accounts_and_refuse_members() {
    admin_forms(false).await;
}

#[tokio::test]
async fn postgres_admin_forms_change_accounts_and_refuse_members() {
    admin_forms(true).await;
}

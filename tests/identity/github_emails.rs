//! Secondary email management uses the real router, mail codes, and both database engines.
use super::{
    email_links::{queued, set_up_mail, token_in},
    github::request,
};
use crate::support::database::Database;
use crate::support::http_client::Client;

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;
use codeyard_storage::ObjectStore;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_identity as identity;
use codeyard_identity::email_links as links;
use codeyard_identity::email_links::Purpose;
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::json;
use std::sync::Arc;

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    f.execute(
        "UPDATE users SET email='alice@example.com',email_verified=TRUE WHERE id=$1",
        &[alice],
    )
    .await;
    f.execute(
        "UPDATE users SET email='bob@example.com',email_verified=TRUE WHERE id=$1",
        &[bob],
    )
    .await;
    set_up_mail(&f).await;
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let session = Client::login(&router, f.pool(), alice).await;
    let bob_session = Client::login(&router, f.pool(), bob).await;
    let mut credentials = Vec::new();
    for (id, session, scope) in [
        (alice, &session.session, "write"),
        (alice, &session.session, "read"),
        (bob, &bob_session.session, "write"),
    ] {
        let created = tokens::create(
            f.pool(),
            id,
            Some(session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", created.value));
    }
    let write = Some(credentials[0].as_str());
    for (operation, method) in [
        ("users/add-email-for-authenticated-user", "POST"),
        ("users/delete-email-for-authenticated-user", "DELETE"),
    ] {
        request(
            &router,
            operation,
            method,
            "/api/v3/user/emails",
            None,
            json!(["work@example.com"]),
            401,
        )
        .await;
        request(
            &router,
            operation,
            method,
            "/api/v3/user/emails",
            Some(&credentials[1]),
            json!(["work@example.com"]),
            403,
        )
        .await;
    }
    for input in [
        json!([]),
        json!({"emails":["new@example.com","invalid"]}),
        json!({"emails":[false]}),
        json!(["new@example.com", "BOB@example.com"]),
    ] {
        request(
            &router,
            "users/add-email-for-authenticated-user",
            "POST",
            "/api/v3/user/emails",
            write,
            input,
            422,
        )
        .await;
        assert!(
            codeyard_db_identity::users::emails::list_emails(f.pool(), alice)
                .await
                .unwrap()
                .is_empty()
        );
    }
    let (_, added) = request(
        &router,
        "users/add-email-for-authenticated-user",
        "POST",
        "/api/v3/user/emails",
        write,
        json!({"emails":["Work@example.com","other@example.com","work@example.com"]}),
        201,
    )
    .await;
    assert_eq!(added.as_array().unwrap().len(), 2);
    for address in added.as_array().unwrap() {
        assert_eq!(address["verified"], false);
        assert_eq!(address["primary"], false);
        assert!(address["visibility"].is_null());
    }
    let mail = queued(f.pool()).await;
    assert_eq!(mail.len(), 2);
    let work = mail
        .iter()
        .find(|mail| mail.to_address == "work@example.com")
        .unwrap();
    let work_code = token_in(work);
    let other_code = token_in(
        mail.iter()
            .find(|mail| mail.to_address == "other@example.com")
            .unwrap(),
    );
    assert_ne!(work_code, other_code);
    for token in [&work_code, &other_code] {
        assert!(links::is_valid(f.pool(), Purpose::Verify, token)
            .await
            .unwrap());
    }
    request(
        &router,
        "users/update-authenticated",
        "PATCH",
        "/api/v3/user",
        write,
        json!({"email":"work@example.com"}),
        422,
    )
    .await;
    let (_, public) = request(
        &router,
        "users/list-public-emails-for-authenticated-user",
        "GET",
        "/api/v3/user/public_emails",
        write,
        json!(null),
        200,
    )
    .await;
    assert_eq!(public, json!([]));
    let (headers, first) = request(
        &router,
        "users/list-emails-for-authenticated-user",
        "GET",
        "/api/v3/user/emails?per_page=1",
        write,
        json!(null),
        200,
    )
    .await;
    assert_eq!(first[0]["primary"], true);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let page = session.page_ok("/settings?tab=email").await;
    assert!(page.contains("/settings/secondary-emails"));
    assert!(page.contains("work@example.com"));
    assert!(page.contains("/settings/email-verification/code"));
    // A primary address already verified must not short-circuit a secondary code.
    let reply = session
        .form_reply("/settings/email-verification/code", &[("code", &work_code)])
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::SEE_OTHER);
    assert!(!links::is_valid(f.pool(), Purpose::Verify, &work_code)
        .await
        .unwrap());
    let (status, _) = session
        .form("/verify-email", &[("token", &other_code)])
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(!links::verify_address(f.pool(), &other_code).await.unwrap());
    let user = codeyard_db_identity::users::accounts::get_user_by_id(f.pool(), alice)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.email.as_deref(), Some("alice@example.com"));
    assert!(user.email_verified);
    // Secondary addresses are not new recovery credentials for the existing account system.
    links::send_password_reset(f.pool(), None, "work@example.com")
        .await
        .unwrap();
    assert!(queued(f.pool()).await.is_empty());
    let (_, profile) = request(
        &router,
        "users/update-authenticated",
        "PATCH",
        "/api/v3/user",
        write,
        json!({"email":"work@example.com"}),
        200,
    )
    .await;
    assert_eq!(profile["email"], "work@example.com");
    let (_, public) = request(
        &router,
        "users/list-public-emails-for-authenticated-user",
        "GET",
        "/api/v3/user/public_emails",
        write,
        json!(null),
        200,
    )
    .await;
    assert_eq!(
        public,
        json!([{"email":"work@example.com","primary":false,"verified":true,"visibility":"public"}])
    );
    assert!(session.page_ok("/alice").await.contains("work@example.com"));
    // A second account cannot claim either a secondary address or a case variant of it.
    request(
        &router,
        "users/add-email-for-authenticated-user",
        "POST",
        "/api/v3/user/emails",
        Some(&credentials[2]),
        json!("WORK@example.com"),
        422,
    )
    .await;
    assert!(
        codeyard_db_identity::users::accounts::create_user_from_oauth(
            f.pool(),
            "collision",
            Some("WORK@example.com"),
            None
        )
        .await
        .is_err()
    );
    request(
        &router,
        "users/delete-email-for-authenticated-user",
        "DELETE",
        "/api/v3/user/emails",
        write,
        json!(["work@example.com", "alice@example.com"]),
        422,
    )
    .await;
    assert_eq!(
        codeyard_db_identity::users::emails::list_emails(f.pool(), alice)
            .await
            .unwrap()
            .len(),
        2
    );
    request(
        &router,
        "users/delete-email-for-authenticated-user",
        "DELETE",
        "/api/v3/user/emails",
        Some(&credentials[2]),
        json!("work@example.com"),
        204,
    )
    .await;
    assert_eq!(
        codeyard_db_identity::users::emails::list_emails(f.pool(), alice)
            .await
            .unwrap()
            .len(),
        2
    );
    let reply = session
        .form_reply(
            "/settings/email-visibility",
            &[("public_address", "other@example.com")],
        )
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::SEE_OTHER);
    let (_, profile) = request(
        &router,
        "users/get-by-username",
        "GET",
        "/api/v3/users/alice",
        None,
        json!(null),
        200,
    )
    .await;
    assert_eq!(profile["email"], "other@example.com");
    let reply = session
        .form_reply(
            "/settings/secondary-emails",
            &[("email", "other@example.com"), ("remove", "yes")],
        )
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::SEE_OTHER);
    let (_, profile) = request(
        &router,
        "users/get-by-username",
        "GET",
        "/api/v3/users/alice",
        None,
        json!(null),
        200,
    )
    .await;
    assert!(profile["email"].is_null());
    // Single strings, arrays, and the documented object all have the same batch semantics.
    request(
        &router,
        "users/add-email-for-authenticated-user",
        "POST",
        "/api/v3/user/emails",
        write,
        json!("expire@example.com"),
        201,
    )
    .await;
    let mail = queued(f.pool()).await;
    assert_eq!(mail.len(), 1);
    let code = token_in(&mail[0]);
    f.execute("UPDATE user_emails SET verification_expires_at='2020-01-01T00:00:00.000000Z' WHERE user_id=$1",&[alice]).await;
    assert!(!links::is_valid(f.pool(), Purpose::Verify, &code)
        .await
        .unwrap());
    assert!(!links::verify_address(f.pool(), &code).await.unwrap());
    request(
        &router,
        "users/delete-email-for-authenticated-user",
        "DELETE",
        "/api/v3/user/emails",
        write,
        json!(["work@example.com", "expire@example.com"]),
        204,
    )
    .await;
    assert!(
        codeyard_db_identity::users::emails::list_emails(f.pool(), alice)
            .await
            .unwrap()
            .is_empty()
    );
    let reply = session
        .form_reply(
            "/settings/secondary-emails",
            &[("secondary_email", "native@example.com")],
        )
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::SEE_OTHER);
    let mail = queued(f.pool()).await;
    assert_eq!(mail.len(), 1);
    let code = token_in(&mail[0]);
    request(
        &router,
        "users/delete-email-for-authenticated-user",
        "DELETE",
        "/api/v3/user/emails",
        write,
        json!("native@example.com"),
        204,
    )
    .await;
    assert!(!links::verify_address(f.pool(), &code).await.unwrap());
    // The database, not just authentication middleware, rejects expired/revoked credentials.
    f.execute(
        "DELETE FROM personal_access_tokens WHERE user_id=$1",
        &[alice],
    )
    .await;
    let result = identity::emails::change(
        f.pool(),
        alice,
        &AccountCredential::Token(i64::MAX, ""),
        &["stale@example.com".into()],
        true,
    )
    .await;
    assert_eq!(result, Err(identity::emails::EmailError::Denied));
    assert!(
        codeyard_db_identity::users::emails::list_emails(f.pool(), alice)
            .await
            .unwrap()
            .is_empty()
    );
    // The settings listing rechecks the requesting session in its own read.
    assert!(codeyard_db_identity::users::emails::list_account_emails(
        f.pool(),
        alice,
        &AccountCredential::Session("ended")
    )
    .await
    .unwrap_err()
    .is::<codeyard_support::credentials::AccountRestricted>());
    assert!(codeyard_db_identity::users::emails::list_account_emails(
        f.pool(),
        alice,
        &AccountCredential::Session(&crate::support::live_session(f.pool(), alice).await)
    )
    .await
    .unwrap()
    .is_empty());
}

#[tokio::test]
async fn sqlite_github_emails() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_emails() {
    contract(true).await;
}

/// One batch charges the mail budget once, so every new address gets its code.
async fn batch_gets_a_code_per_address(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let carol = f.actor("carol").await;
    f.execute(
        "UPDATE users SET email='carol@example.com',email_verified=TRUE WHERE id=$1",
        &[carol],
    )
    .await;
    set_up_mail(&f).await;
    codeyard_db_identity::users::sessions::create_session(
        f.pool(),
        "batch-session",
        carol,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let emails: Vec<String> = (1..=11).map(|n| format!("batch{n}@example.com")).collect();
    identity::emails::change(
        f.pool(),
        carol,
        &AccountCredential::Session("batch-session"),
        &emails,
        true,
    )
    .await
    .unwrap();
    assert_eq!(queued(f.pool()).await.len(), 11);
}

#[tokio::test]
async fn sqlite_secondary_batch_gets_a_code_per_address() {
    batch_gets_a_code_per_address(false).await;
}
#[tokio::test]
async fn postgres_secondary_batch_gets_a_code_per_address() {
    batch_gets_a_code_per_address(true).await;
}

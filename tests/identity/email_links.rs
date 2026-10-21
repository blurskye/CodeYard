//! Codes and links sent by email, on SQLite and PostgreSQL: confirming an address and resetting
//! a password. Mail is only queued here; nothing talks to an SMTP server.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use crate::support::http_client::{visible_text, Client};
use crate::support::PASSWORD_WORK as SERIAL;
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use codeyard::backend::{
    db::{
        self,
        email::NewEmailSettings,
        models::{OutboxMessage, User},
        DbPool,
    },
    email::links::{self, LinkError, Purpose},
    identity::{
        self,
        accounts::{PasswordChange, PasswordResetError},
        admission, passwords,
    },
};

const PAST: &str = "2020-01-01T00:00:00.000000Z";

async fn set_up_mail(f: &Database) {
    let admin = f.actor("mail_admin").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[admin])
        .await;
    db::users::sessions::create_session(
        f.pool(),
        "mail-admin-session",
        admin,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    let settings = NewEmailSettings {
        host: "mail.example.test",
        port: 2525,
        security: "none",
        username: "",
        password: None,
        from_name: "Codeyard",
        from_address: "noreply@example.test",
        reply_to: "",
    };
    assert!(
        db::email::save_email_settings(f.pool(), admin, "mail-admin-session", &settings)
            .await
            .unwrap()
    );
}

async fn queued(pool: &DbPool) -> Vec<OutboxMessage> {
    let mut mail = Vec::new();
    while let Some(message) = db::email::claim_due_email(pool, 5, 600).await.unwrap() {
        mail.push(message);
    }
    mail
}

/// The code on its own line under "Your code:". It is the token a link carries when PUBLIC_URL is
/// set, so it opens the link pages too.
fn token_in(message: &OutboxMessage) -> String {
    let code = message
        .body_text
        .strip_prefix("Your code:\n")
        .and_then(|rest| rest.lines().next())
        .expect("a code")
        .to_owned();
    assert_eq!(code.len(), 64, "{}", message.body_text);
    code
}

/// Makes the user's links look half an hour from expiry, so a new request may replace them.
async fn age_links(f: &Database, user: i64) {
    let later = (Utc::now() + Duration::minutes(30)).format("%Y-%m-%dT%H:%M:%S%.6fZ");
    f.execute(
        &format!("UPDATE email_tokens SET expires_at='{later}' WHERE user_id=$1"),
        &[user],
    )
    .await;
}

async fn user_named(pool: &DbPool, name: &str) -> User {
    db::users::accounts::get_user_by_username(pool, name)
        .await
        .unwrap()
        .unwrap()
}

async fn expire_links(f: &Database, user: i64) {
    f.execute(
        &format!("UPDATE email_tokens SET expires_at='{PAST}' WHERE user_id=$1"),
        &[user],
    )
    .await;
}

async fn verification_links(pg: bool) {
    let _serial = SERIAL.lock().await;
    let mut f = Database::new(pg).await;
    let pool = f.pool();
    let hash = passwords::hash_password("correct-password").await.unwrap();

    // Without a mail server nothing is queued.
    let carol = db::users::accounts::create_user(pool, "carol", Some("carol@example.test"))
        .await
        .unwrap();
    assert_eq!(
        links::send_verification(pool, &carol).await,
        Err(LinkError::NotConfigured)
    );
    assert!(queued(pool).await.is_empty());

    set_up_mail(&f).await;

    // The link confirms the address once; the same token does not work as a reset link.
    links::send_verification(pool, &carol).await.unwrap();
    let mail = queued(pool).await;
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0].to_address, "carol@example.test");
    let token = token_in(&mail[0]);
    assert!(links::is_valid(pool, Purpose::Verify, &token)
        .await
        .unwrap());
    assert!(!links::is_valid(pool, Purpose::Reset, &token).await.unwrap());
    assert!(!user_named(pool, "carol").await.email_verified);
    assert!(links::verify_address(pool, &token).await.unwrap());
    assert!(user_named(pool, "carol").await.email_verified);
    assert!(!links::verify_address(pool, &token).await.unwrap());

    // A resend within a minute mails nothing; a later one replaces the earlier link and code.
    let dave = db::users::accounts::create_user(pool, "dave", Some("dave@example.test"))
        .await
        .unwrap();
    links::send_verification(pool, &dave).await.unwrap();
    links::send_verification(pool, &dave).await.unwrap();
    let mail = queued(pool).await;
    assert_eq!(mail.len(), 1, "a resend within a minute mails nothing");
    let first = token_in(&mail[0]);
    age_links(&f, dave.id).await;
    links::send_verification(pool, &dave).await.unwrap();
    let mail = queued(pool).await;
    assert_eq!(mail.len(), 1);
    let second = token_in(&mail[0]);
    assert_ne!(first, second);
    assert!(!links::verify_address(pool, &first).await.unwrap());
    assert!(links::verify_address(pool, &second).await.unwrap());

    // Without PUBLIC_URL the mail carries only the code, and pasting it confirms the address.
    let hana = db::users::accounts::create_user(pool, "hana", Some("hana@example.test"))
        .await
        .unwrap();
    links::send_verification(pool, &hana).await.unwrap();
    let mail = queued(pool).await;
    if std::env::var_os("PUBLIC_URL").is_none() {
        assert!(
            !mail[0].body_text.contains("token="),
            "{}",
            mail[0].body_text
        );
    }
    let code = token_in(&mail[0]);
    assert!(links::verify_address(pool, &code).await.unwrap());
    assert!(user_named(pool, "hana").await.email_verified);
    assert!(!links::verify_address(pool, &code).await.unwrap());

    // An expired link confirms nothing.
    let erin = db::users::accounts::create_user(pool, "erin", Some("erin@example.test"))
        .await
        .unwrap();
    links::send_verification(pool, &erin).await.unwrap();
    let mail = queued(pool).await;
    let token = token_in(&mail[0]);
    expire_links(&f, erin.id).await;
    assert!(!links::is_valid(pool, Purpose::Verify, &token)
        .await
        .unwrap());
    assert!(!links::verify_address(pool, &token).await.unwrap());
    assert!(!user_named(pool, "erin").await.email_verified);

    // A link sent to an address the user has since left confirms nothing.
    let frank = db::users::accounts::create_user_with_password(
        pool,
        "frank",
        Some("frank@example.test"),
        &hash,
    )
    .await
    .unwrap();
    db::users::sessions::create_session(
        pool,
        "frank-session",
        frank.id,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    links::send_verification(pool, &frank).await.unwrap();
    let token = token_in(&queued(pool).await[0]);
    db::users::transitions::change_email(
        pool,
        frank.id,
        "frank-new@example.test",
        &hash,
        "frank-session",
    )
    .await
    .unwrap();
    assert!(!links::is_valid(pool, Purpose::Verify, &token)
        .await
        .unwrap());
    assert!(!links::verify_address(pool, &token).await.unwrap());
    assert!(!user_named(pool, "frank").await.email_verified);

    // Changing the address clears the flag and mails a link to the new address.
    f.execute(
        "UPDATE users SET email_verified=TRUE WHERE id=$1",
        &[frank.id],
    )
    .await;
    identity::accounts::change_email(
        pool,
        Some("frank-session"),
        "correct-password",
        "frank-third@example.test",
    )
    .await
    .unwrap();
    assert!(!user_named(pool, "frank").await.email_verified);
    let mail = queued(pool).await;
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0].to_address, "frank-third@example.test");
    assert!(links::verify_address(pool, &token_in(&mail[0]))
        .await
        .unwrap());
    assert!(user_named(pool, "frank").await.email_verified);

    // Requests are limited per user.
    let gina = db::users::accounts::create_user(pool, "gina", Some("gina@example.test"))
        .await
        .unwrap();
    for _ in 0..10 {
        links::send_verification(pool, &gina).await.unwrap();
    }
    assert_eq!(
        links::send_verification(pool, &gina).await,
        Err(LinkError::Limited)
    );

    // An account without an address has nothing to confirm.
    let none = db::users::accounts::create_user(pool, "no_address", None)
        .await
        .unwrap();
    assert_eq!(
        links::send_verification(pool, &none).await,
        Err(LinkError::NoAddress)
    );

    f.finish().await;
}

async fn reset_links(pg: bool) {
    let _serial = SERIAL.lock().await;
    let mut f = Database::new(pg).await;
    let pool = f.pool();
    let hash = passwords::hash_password("old-password-1").await.unwrap();
    set_up_mail(&f).await;
    let ivan = db::users::accounts::create_user_with_password(
        pool,
        "ivan",
        Some("ivan@example.test"),
        &hash,
    )
    .await
    .unwrap();

    // An unverified address gets no link, and the answer is the same as for a stranger.
    links::send_password_reset(pool, None, "ivan@example.test")
        .await
        .unwrap();
    links::send_password_reset(pool, None, "nobody@example.test")
        .await
        .unwrap();
    assert!(queued(pool).await.is_empty());

    // An account with no password has nothing to reset.
    let oauth = db::users::accounts::create_user(pool, "oauth_only", Some("oauth@example.test"))
        .await
        .unwrap();
    f.execute(
        "UPDATE users SET email_verified=TRUE WHERE id=$1",
        &[oauth.id],
    )
    .await;
    links::send_password_reset(pool, None, "oauth@example.test")
        .await
        .unwrap();
    assert!(queued(pool).await.is_empty());

    f.execute(
        "UPDATE users SET email_verified=TRUE WHERE id=$1",
        &[ivan.id],
    )
    .await;
    db::users::sessions::create_session(
        pool,
        "ivan-session",
        ivan.id,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    links::send_password_reset(pool, None, "ivan@example.test")
        .await
        .unwrap();
    let mail = queued(pool).await;
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0].to_address, "ivan@example.test");
    assert!(mail[0].subject.contains("Reset"));
    let token = token_in(&mail[0]);
    // A reset link does not confirm the address.
    assert!(!links::verify_address(pool, &token).await.unwrap());

    // A second request within a minute mails nothing, and the first link keeps working.
    links::send_password_reset(pool, None, "ivan@example.test")
        .await
        .unwrap();
    assert!(queued(pool).await.is_empty());
    assert!(links::is_valid(pool, Purpose::Reset, &token).await.unwrap());

    // Sent mail keeps no body, so the link is not left in the outbox.
    db::email::mark_email_sent(pool, mail[0].id).await.unwrap();
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM email_outbox WHERE id=$1 AND body_text=''",
            &[mail[0].id]
        )
        .await,
        1
    );

    // A password that is too short leaves the link usable.
    assert!(matches!(
        identity::accounts::reset_password(pool, &token, "short").await,
        Err(PasswordResetError::Input(_))
    ));
    assert!(links::is_valid(pool, Purpose::Reset, &token).await.unwrap());

    identity::accounts::reset_password(pool, &token, "brand-new-password")
        .await
        .unwrap();
    let ivan = user_named(pool, "ivan").await;
    assert!(
        passwords::verify_password("brand-new-password", ivan.password_hash.as_deref())
            .await
            .unwrap()
            .valid
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM sessions WHERE user_id=$1", &[ivan.id])
            .await,
        0,
        "a reset ends every session"
    );

    // The link works once.
    assert_eq!(
        identity::accounts::reset_password(pool, &token, "another-password").await,
        Err(PasswordResetError::InvalidLink)
    );

    // An expired code does nothing.
    links::send_password_reset(pool, None, "ivan@example.test")
        .await
        .unwrap();
    let code = token_in(&queued(pool).await[0]);
    expire_links(&f, ivan.id).await;
    assert_eq!(
        identity::accounts::reset_password(pool, &code, "another-password").await,
        Err(PasswordResetError::InvalidLink)
    );

    // The pasted code sets a new password once, like the link.
    links::send_password_reset(pool, None, "ivan@example.test")
        .await
        .unwrap();
    let code = token_in(&queued(pool).await[0]);
    identity::accounts::reset_password(pool, &code, "code-password")
        .await
        .unwrap();
    let ivan = user_named(pool, "ivan").await;
    assert!(
        passwords::verify_password("code-password", ivan.password_hash.as_deref())
            .await
            .unwrap()
            .valid
    );
    assert_eq!(
        identity::accounts::reset_password(pool, &code, "other-password").await,
        Err(PasswordResetError::InvalidLink)
    );

    // Changing the password in Settings cancels a reset link that is still in the mailbox.
    links::send_password_reset(pool, None, "ivan@example.test")
        .await
        .unwrap();
    let token = token_in(&queued(pool).await[0]);
    db::users::sessions::create_session(
        pool,
        "ivan-session-2",
        ivan.id,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    identity::accounts::change_password(
        pool,
        Some("ivan-session-2"),
        PasswordChange {
            current_password: "code-password",
            new_password: "changed-password",
        },
    )
    .await
    .unwrap();
    assert!(!links::is_valid(pool, Purpose::Reset, &token).await.unwrap());

    // Mail requests from one client address have their own budget, so they cannot lock that
    // address out of signing in.
    let client = Some("203.0.113.50");
    for n in 0..30 {
        links::send_password_reset(pool, client, &format!("stranger{n}@example.test"))
            .await
            .unwrap();
    }
    assert_eq!(
        links::send_password_reset(pool, client, "stranger@example.test").await,
        Err(LinkError::Limited)
    );
    admission::admit_login(pool, client, Some(ivan.id), "ivan")
        .await
        .unwrap();

    // Requests for one address are limited, whether or not the address exists.
    for _ in 0..9 {
        links::send_password_reset(pool, None, "nobody@example.test")
            .await
            .unwrap();
    }
    assert_eq!(
        links::send_password_reset(pool, None, "nobody@example.test").await,
        Err(LinkError::Limited)
    );

    f.finish().await;
}

async fn email_link_pages(pg: bool) {
    let _serial = SERIAL.lock().await;
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let pool = f.pool();
    let hash = passwords::hash_password("old-password-1").await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let kate = db::users::accounts::create_user_with_password(
        pool,
        "kate",
        Some("kate@example.test"),
        &hash,
    )
    .await
    .unwrap();
    let client = Client::login(&router, pool, kate.id).await;

    // Without a mail server the pages say so, and the settings page says verification is off.
    let (status, page) = client.page("/forgot-password").await;
    assert_eq!(status, StatusCode::OK);
    assert!(visible_text(&page).contains("cannot send email"), "{page}");
    assert!(!page.contains("action=\"/forgot-password\""));
    let settings = client.page_ok("/settings?tab=email").await;
    assert!(
        visible_text(&settings).contains("Verification is unavailable"),
        "{settings}"
    );
    assert!(!settings.contains("Resend verification email"));

    set_up_mail(&f).await;
    let settings = client.page_ok("/settings?tab=email").await;
    assert!(settings.contains("Resend verification email"), "{settings}");

    // The resend button mails a link; opening the link confirms nothing until its form is sent.
    let reply = client.form_reply("/settings/email-verification", &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.location.as_deref(),
        Some("/settings?tab=email&saved=verification-sent")
    );
    let mail = queued(pool).await;
    assert_eq!(mail.len(), 1);
    let token = token_in(&mail[0]);
    // Settings takes the code from the mail as well; a wrong one is refused.
    let settings = client.page_ok("/settings?tab=email").await;
    assert!(
        settings.contains("action=\"/settings/email-verification/code\""),
        "{settings}"
    );
    let (status, page) = client
        .form(
            "/settings/email-verification/code",
            &[("code", "not-the-code")],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(visible_text(&page).contains("This code is wrong"), "{page}");
    assert!(!user_named(pool, "kate").await.email_verified);
    let verify_path = format!("/verify-email?token={token}");
    let page = client.page_ok(&verify_path).await;
    assert!(page.contains("Confirm email address"), "{page}");
    assert!(!user_named(pool, "kate").await.email_verified);
    let (status, page) = client.page("/verify-email?token=wrong").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(visible_text(&page).contains("invalid"), "{page}");

    // A form without the page's token is refused.
    let reply = client
        .exchange(
            "POST",
            "/verify-email",
            "application/x-www-form-urlencoded",
            format!("token={token}").into_bytes(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(!user_named(pool, "kate").await.email_verified);

    let (status, page) = client.form("/verify-email", &[("token", &token)]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        visible_text(&page).contains("Your email address is confirmed."),
        "{page}"
    );
    assert!(user_named(pool, "kate").await.email_verified);
    let (status, _) = client.form("/verify-email", &[("token", &token)]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The code pasted into Settings with spaces around it confirms the address.
    let lena = db::users::accounts::create_user_with_password(
        pool,
        "lena",
        Some("lena@example.test"),
        &hash,
    )
    .await
    .unwrap();
    let lena_client = Client::login(&router, pool, lena.id).await;
    let reply = lena_client
        .form_reply("/settings/email-verification", &[])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let pasted = format!("  {}\n", token_in(&queued(pool).await[0]));
    let reply = lena_client
        .form_reply("/settings/email-verification/code", &[("code", &pasted)])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.location.as_deref(),
        Some("/settings?tab=email&saved=email-verified")
    );
    assert!(user_named(pool, "lena").await.email_verified);

    // The reset request answers alike for a known and an unknown address.
    let page = client.page_ok("/forgot-password").await;
    assert!(page.contains("action=\"/forgot-password\""), "{page}");
    let known = client
        .form("/forgot-password", &[("email", "kate@example.test")])
        .await;
    let unknown = client
        .form("/forgot-password", &[("email", "nobody@example.test")])
        .await;
    assert_eq!(known.0, StatusCode::OK);
    assert_eq!(known.0, unknown.0);
    assert!(visible_text(&known.1).contains("on its way"), "{}", known.1);
    assert!(visible_text(&unknown.1).contains("on its way"));
    let mail = queued(pool).await;
    assert_eq!(mail.len(), 1, "only the known address gets mail");
    let token = token_in(&mail[0]);

    // The reset page also waits for its form; a mismatch keeps the link, a match changes the
    // password and sends the user to sign in.
    let reset_path = format!("/reset-password?token={token}");
    let page = client.page_ok(&reset_path).await;
    assert!(page.contains("Change password"), "{page}");
    let (status, _) = client
        .form(
            "/reset-password",
            &[
                ("token", &token),
                ("new_password", "brand-new-password"),
                ("confirm_password", "different-password"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let reply = client
        .form_reply(
            "/reset-password",
            &[
                ("token", &token),
                ("new_password", "brand-new-password"),
                ("confirm_password", "brand-new-password"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/login?reset=done"));
    let kate = user_named(pool, "kate").await;
    assert!(
        passwords::verify_password("brand-new-password", kate.password_hash.as_deref())
            .await
            .unwrap()
            .valid
    );
    let (status, page) = client.page(&reset_path).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(visible_text(&page).contains("Ask for a new link"), "{page}");

    // The reset request leads to the code step. A wrong code gets one generic answer, and the
    // right one, pasted with spaces around it, changes the password and sends the user to sign in.
    let client = Client::login(&router, pool, kate.id).await;
    let (status, page) = client
        .form("/forgot-password", &[("email", "kate@example.test")])
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(visible_text(&page).contains("Check your email"), "{page}");
    assert!(page.contains("action=\"/reset-password/code\""), "{page}");
    assert!(page.contains("value=\"kate@example.test\""), "{page}");
    let code = token_in(&queued(pool).await[0]);
    let code_form = |code: String| {
        vec![
            ("email", "kate@example.test".to_owned()),
            ("code", code),
            ("new_password", "code-password-1".to_owned()),
            ("confirm_password", "code-password-1".to_owned()),
        ]
    };
    let pairs = code_form("not-the-code".to_owned());
    let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (status, page) = client.form("/reset-password/code", &pairs).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(visible_text(&page).contains("This code is wrong"), "{page}");
    let pairs = code_form(format!(" {code}\n"));
    let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let reply = client.form_reply("/reset-password/code", &pairs).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/login?reset=done"));
    let kate = user_named(pool, "kate").await;
    assert!(
        passwords::verify_password("code-password-1", kate.password_hash.as_deref())
            .await
            .unwrap()
            .valid
    );

    f.finish().await;
}

#[tokio::test]
async fn sqlite_verification_links_confirm_the_current_address_once() {
    verification_links(false).await;
}

#[tokio::test]
async fn postgres_verification_links_confirm_the_current_address_once() {
    verification_links(true).await;
}

#[tokio::test]
async fn sqlite_reset_links_set_a_new_password_once() {
    reset_links(false).await;
}

#[tokio::test]
async fn postgres_reset_links_set_a_new_password_once() {
    reset_links(true).await;
}

#[tokio::test]
async fn sqlite_email_link_pages_wait_for_their_forms() {
    email_link_pages(false).await;
}

#[tokio::test]
async fn postgres_email_link_pages_wait_for_their_forms() {
    email_link_pages(true).await;
}

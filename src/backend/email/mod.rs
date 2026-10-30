//! Outgoing email over SMTP. The server settings live in `config`, the texts in `messages`,
//! `links` mails the address confirmation and password reset codes, and `outbox` sends the
//! queued mail. The entry points below queue what the site mails.
pub mod config;
pub mod links;
mod messages;
pub mod outbox;
pub mod send;

use crate::backend::api::http::config::public_origin;
use crate::backend::crypto::SecretEncryptor;
use crate::backend::db::{self, DbPool};
use tracing::warn;

/// Mails a notification to its recipient, if the site has a mail server and the recipient has a
/// verified address and wants email.
pub async fn notify_user(pool: &DbPool, notification: &db::models::Notification) {
    if !config::is_configured(pool).await {
        return;
    }
    match db::email::notification_email_address(pool, notification.user_id).await {
        Ok(Some(to)) => {
            let mail = messages::notification(
                &public_origin(),
                &notification.title,
                notification.body.as_deref(),
                notification.link.as_deref(),
            );
            if let Err(error) = db::email::enqueue_notification_email(
                pool,
                notification.id,
                &to,
                &mail.subject,
                &mail.body,
            )
            .await
            {
                warn!(%error, "could not queue an email");
            }
        }
        Ok(None) => {}
        Err(error) => warn!(%error, "could not look up the address for a notification email"),
    }
}

/// Invitation messages are linked to their offer so exhausted SMTP retries can be reported.
pub async fn organization_invitation(
    pool: &DbPool,
    invitation: &db::models::OrgInvitation,
    name: &str,
) {
    if !config::is_configured(pool).await {
        return;
    }
    let to = match &invitation.email {
        Some(email) => Some(email.clone()),
        None => match invitation.user_id {
            Some(user) => db::email::notification_email_address(pool, user)
                .await
                .ok()
                .flatten(),
            None => None,
        },
    };
    let Some(to) = to else {
        return;
    };
    let title = format!("Invitation to join {name}");
    let body = format!("You have been invited to join {name}. Sign in, or create an account and verify this email address, to accept the invitation at {}/settings/organizations.\n\nThis invitation expires after seven days.", public_origin());
    if let Err(error) =
        db::email::enqueue_invitation_email(pool, invitation.id, &to, &title, &body).await
    {
        warn!(%error, "could not queue an organization invitation email");
    }
}

/// Tells the person who asked for an account that an administrator decided on the request.
pub async fn signup_decision(pool: &DbPool, email: Option<&str>, username: &str, approved: bool) {
    let Some(to) = email.map(str::trim).filter(|email| !email.is_empty()) else {
        return;
    };
    if config::is_configured(pool).await {
        let mail = messages::signup_decision(&public_origin(), username, approved);
        outbox::enqueue(pool, to, mail).await;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TestMailError {
    #[error("A live unrestricted administrator session is required")]
    Authority(crate::backend::identity::sessions::AccountAuthorityError),
    #[error("Your account has no email address. Add one in Settings under Email address.")]
    NoAddress,
    #[error("{0}")]
    Delivery(String),
}

/// Rechecks the initiating administrator after configuration loading, at send admission.
pub async fn send_test(
    pool: &DbPool,
    secrets: Option<&SecretEncryptor>,
    session: &str,
) -> Result<String, TestMailError> {
    let (config, _) = config::load(pool, secrets).await.map_err(TestMailError::Delivery)?
        .ok_or_else(|| TestMailError::Delivery("Email is not set up. Save the settings first.".into()))?;
    let admin = crate::backend::identity::sessions::administrator(pool, Some(session)).await
        .map_err(TestMailError::Authority)?;
    let to = admin.email.as_deref().map(str::trim).filter(|email| !email.is_empty()).ok_or(TestMailError::NoAddress)?;
    let mail = messages::test_message();
    send::send(&config, to, &mail.subject, &mail.body).await.map_err(TestMailError::Delivery)?;
    Ok(to.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::db::email::NewEmailSettings;

    struct Fixture {
        _directory: tempfile::TempDir,
        pool: DbPool,
        admin: i64,
        secrets: SecretEncryptor,
    }

    async fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let pool = db::init_db(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        db::run_migrations(&pool).await.unwrap();
        let admin = db::users::accounts::create_user(&pool, "admin", Some("admin@example.com"))
            .await
            .unwrap();
        let DbPool::Sqlite(raw) = &pool else {
            unreachable!()
        };
        sqlx::query("UPDATE users SET is_admin=1, email_verified=1 WHERE id=?1")
            .bind(admin.id)
            .execute(raw)
            .await
            .unwrap();
        sqlx::query("INSERT INTO sessions(id,user_id,expires_at) VALUES ('live',?1,'2999-01-01T00:00:00.000000Z')")
            .bind(admin.id)
            .execute(raw)
            .await
            .unwrap();
        Fixture {
            _directory: directory,
            pool,
            admin: admin.id,
            secrets: SecretEncryptor::new(&[7; 32]),
        }
    }

    fn settings<'a>(host: &'a str, password: Option<(&'a [u8], &'a [u8])>) -> NewEmailSettings<'a> {
        NewEmailSettings {
            host,
            port: 2525,
            security: "tls",
            username: "mailer",
            password,
            from_name: "Codeyard",
            from_address: "noreply@example.com",
            reply_to: "",
        }
    }

    #[tokio::test]
    async fn saved_settings_round_trip_and_keep_the_password() {
        let f = fixture().await;
        assert!(config::load(&f.pool, None).await.unwrap().is_none());
        assert!(!config::is_configured(&f.pool).await);

        let (ciphertext, nonce) = f.secrets.encrypt(b"s3cret").unwrap();
        let first = settings(
            "mail.example.com",
            Some((ciphertext.as_slice(), nonce.as_slice())),
        );
        assert!(
            db::email::save_email_settings(&f.pool, f.admin, "live", &first)
                .await
                .unwrap()
        );
        let (saved, source) = config::load(&f.pool, Some(&f.secrets))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source, config::Source::Database);
        assert_eq!(saved.host, "mail.example.com");
        assert_eq!((saved.port, saved.security), (2525, config::Security::Tls));
        assert_eq!(saved.password.as_deref(), Some("s3cret"));
        assert!(config::is_configured(&f.pool).await);

        // Saving without a password keeps the one already stored.
        let second = settings("smtp.example.org", None);
        assert!(
            db::email::save_email_settings(&f.pool, f.admin, "live", &second)
                .await
                .unwrap()
        );
        let (saved, _) = config::load(&f.pool, Some(&f.secrets))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.host, "smtp.example.org");
        assert_eq!(saved.password.as_deref(), Some("s3cret"));

        // Without the master key the saved password cannot be opened.
        assert!(config::load(&f.pool, None).await.is_err());
        let wrong = SecretEncryptor::new(&[8; 32]);
        assert!(config::load(&f.pool, Some(&wrong)).await.is_err());
    }

    #[tokio::test]
    async fn only_a_signed_in_administrator_can_save() {
        let f = fixture().await;
        let input = settings("mail.example.com", None);
        assert!(
            !db::email::save_email_settings(&f.pool, f.admin, "expired", &input)
                .await
                .unwrap()
        );
        let user = db::users::accounts::create_user(&f.pool, "bob", None)
            .await
            .unwrap();
        assert!(
            !db::email::save_email_settings(&f.pool, user.id, "live", &input)
                .await
                .unwrap()
        );
        assert!(db::email::get_email_settings(&f.pool)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn notification_mail_needs_a_verified_address_and_the_preference() {
        let f = fixture().await;
        let unverified = db::users::accounts::create_user(&f.pool, "bob", Some("bob@example.com"))
            .await
            .unwrap();
        assert_eq!(
            db::email::notification_email_address(&f.pool, f.admin)
                .await
                .unwrap()
                .as_deref(),
            Some("admin@example.com")
        );
        assert_eq!(
            db::email::notification_email_address(&f.pool, unverified.id)
                .await
                .unwrap(),
            None
        );

        assert!(db::email::get_email_notifications(&f.pool, f.admin, &db::users::accounts::AccountCredential::Session("live"))
            .await
            .unwrap());
        db::email::set_email_notifications(&f.pool, f.admin, &db::users::accounts::AccountCredential::Session("live"), false)
            .await
            .unwrap();
        assert!(!db::email::get_email_notifications(&f.pool, f.admin, &db::users::accounts::AccountCredential::Session("live"))
            .await
            .unwrap());
        assert_eq!(
            db::email::notification_email_address(&f.pool, f.admin)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn nothing_is_queued_without_a_mail_server() {
        let f = fixture().await;
        static NO_METADATA: serde_json::Value = serde_json::Value::Null;
        let notification = |body: Option<&'static str>, link: Option<&'static str>| {
            db::notifications::records::create_notification(
                &f.pool,
                db::notifications::records::NewNotification {
                    user_id: f.admin,
                    actor_id: None,
                    repo_id: None,
                    kind: "mention",
                    title: "Title",
                    body,
                    link,
                    metadata: &NO_METADATA,
                },
            )
        };
        notify_user(&f.pool, &notification(None, None).await.unwrap()).await;
        signup_decision(&f.pool, Some("carol@example.com"), "carol", true).await;
        assert!(db::email::claim_due_email(&f.pool, 5, 600)
            .await
            .unwrap()
            .is_none());

        let input = settings("mail.example.com", None);
        db::email::save_email_settings(&f.pool, f.admin, "live", &input)
            .await
            .unwrap();
        let mail = notification(Some("Body"), Some("/x")).await.unwrap();
        notify_user(&f.pool, &mail).await;
        signup_decision(&f.pool, Some("carol@example.com"), "carol", false).await;
        let first = db::email::claim_due_email(&f.pool, 5, 600)
            .await
            .unwrap()
            .unwrap();
        let second = db::email::claim_due_email(&f.pool, 5, 600)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (first.to_address.as_str(), first.subject.as_str()),
            ("admin@example.com", "Title")
        );
        assert_eq!(first.notification_id, Some(mail.id));
        assert!(
            db::notifications::records::notification_email_allowed(&f.pool, mail.id)
                .await
                .unwrap()
        );
        assert_eq!(second.to_address, "carol@example.com");
    }

    #[tokio::test]
    async fn the_outbox_claims_retries_gives_up_and_prunes() {
        let f = fixture().await;
        let DbPool::Sqlite(raw) = &f.pool else {
            unreachable!()
        };
        db::email::enqueue_email(&f.pool, "a@example.com", "One", "Body")
            .await
            .unwrap();

        // Two claims at once get the one message between them.
        let (left, right) = tokio::join!(
            db::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 600),
            db::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 600),
        );
        let claimed: Vec<_> = [left.unwrap(), right.unwrap()]
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].attempts, 1);
        // A claimed message is held, so it is not handed out again.
        assert!(
            db::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 600)
                .await
                .unwrap()
                .is_none()
        );

        // Each failure sets the next attempt; with no wait it is due again at once.
        let id = claimed[0].id;
        for attempts in 1..=outbox::MAX_ATTEMPTS {
            db::email::mark_email_failed(&f.pool, id, attempts, "refused", 0, outbox::MAX_ATTEMPTS)
                .await
                .unwrap();
            let next = db::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 600)
                .await
                .unwrap();
            if attempts == outbox::MAX_ATTEMPTS {
                assert!(
                    next.is_none(),
                    "gives up after {} attempts",
                    outbox::MAX_ATTEMPTS
                );
            } else {
                assert_eq!(next.unwrap().attempts, attempts + 1);
            }
        }
        let (error, body): (String, String) =
            sqlx::query_as("SELECT last_error, body_text FROM email_outbox WHERE id=?1")
                .bind(id)
                .fetch_one(raw)
                .await
                .unwrap();
        assert_eq!(error, "refused");
        assert_eq!(body, "", "a given-up message keeps no body");

        // A failure with a wait is not due yet.
        db::email::enqueue_email(&f.pool, "b@example.com", "Two", "Body")
            .await
            .unwrap();
        let two = db::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 600)
            .await
            .unwrap()
            .unwrap();
        db::email::mark_email_failed(&f.pool, two.id, two.attempts, "later", 3600, outbox::MAX_ATTEMPTS)
            .await
            .unwrap();
        assert!(
            db::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 600)
                .await
                .unwrap()
                .is_none()
        );

        // Sent mail and given-up mail are pruned once old; waiting mail stays.
        db::email::enqueue_email(&f.pool, "c@example.com", "Three", "Body")
            .await
            .unwrap();
        sqlx::query("UPDATE email_outbox SET sent_at=created_at WHERE subject='Three'")
            .execute(raw)
            .await
            .unwrap();
        sqlx::query("UPDATE email_outbox SET created_at='2020-01-01T00:00:00.000000Z'")
            .execute(raw)
            .await
            .unwrap();
        assert_eq!(
            db::email::remove_old_email(&f.pool, outbox::MAX_ATTEMPTS, 14)
                .await
                .unwrap(),
            2
        );
        let left: Vec<String> = sqlx::query_scalar("SELECT subject FROM email_outbox")
            .fetch_all(raw)
            .await
            .unwrap();
        assert_eq!(left, ["Two"]);

        // A sender that dies mid-send still uses up the attempts, and the message is given up.
        db::email::enqueue_email(&f.pool, "d@example.com", "Four", "Body")
            .await
            .unwrap();
        for attempt in 1..=outbox::MAX_ATTEMPTS {
            let claimed = db::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 0)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(claimed.attempts, attempt);
        }
        assert!(db::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 0)
            .await
            .unwrap()
            .is_none());
        let (error, body): (String, String) =
            sqlx::query_as("SELECT last_error, body_text FROM email_outbox WHERE subject='Four'")
                .fetch_one(raw)
                .await
                .unwrap();
        assert_eq!(
            (error.as_str(), body.as_str()),
            ("The send did not finish", "")
        );
    }
}

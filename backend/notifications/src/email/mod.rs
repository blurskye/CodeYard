//! Outgoing email over SMTP. The server settings live in `config`, the texts in `messages`,
//! `links` mails the address confirmation and password reset codes, and `outbox` sends the
//! queued mail. The entry points below queue what the site mails, or write the mail that a
//! database transaction queues with the change it reports.

pub mod outbox;
pub mod send;

use codeyard_model::http_config::public_origin;
use codeyard_support::secrets::SecretEncryptor;

use codeyard_db_core::DbPool;
use tracing::warn;

/// Mails a notification to its recipient, if the site has a mail server and the recipient has a
/// verified address and wants email.
pub async fn notify_user(pool: &DbPool, notification: &codeyard_db_core::models::Notification) {
    if !config::is_configured(pool).await {
        return;
    }
    match codeyard_db_identity::email::notification_email_address(pool, notification.user_id).await
    {
        Ok(Some(to)) => {
            let mail = codeyard_model::mail_messages::notification(
                &public_origin(),
                &notification.title,
                notification.body.as_deref(),
                notification.link.as_deref(),
            );
            if let Err(error) = codeyard_db_identity::email::enqueue_notification_email(
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

#[cfg(test)]
mod tests {
    use super::*;
    use codeyard_db_identity::email::NewEmailSettings;

    struct Fixture {
        _directory: tempfile::TempDir,
        pool: DbPool,
        admin: i64,
        secrets: SecretEncryptor,
    }

    async fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let pool = codeyard_db_core::init_db(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        codeyard_db_core::run_migrations(&pool).await.unwrap();
        let admin = codeyard_db_identity::users::accounts::create_user(
            &pool,
            "admin",
            Some("admin@example.com"),
        )
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
            codeyard_db_identity::email::save_email_settings(&f.pool, f.admin, "live", &first)
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
        assert!(codeyard_db_identity::email::save_email_settings(
            &f.pool, f.admin, "live", &second
        )
        .await
        .unwrap());
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
        assert!(!codeyard_db_identity::email::save_email_settings(
            &f.pool, f.admin, "expired", &input
        )
        .await
        .unwrap());
        let user = codeyard_db_identity::users::accounts::create_user(&f.pool, "bob", None)
            .await
            .unwrap();
        assert!(!codeyard_db_identity::email::save_email_settings(
            &f.pool, user.id, "live", &input
        )
        .await
        .unwrap());
        assert!(codeyard_db_identity::email::get_email_settings(&f.pool)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn notification_mail_needs_a_verified_address_and_the_preference() {
        let f = fixture().await;
        let unverified = codeyard_db_identity::users::accounts::create_user(
            &f.pool,
            "bob",
            Some("bob@example.com"),
        )
        .await
        .unwrap();
        assert_eq!(
            codeyard_db_identity::email::notification_email_address(&f.pool, f.admin)
                .await
                .unwrap()
                .as_deref(),
            Some("admin@example.com")
        );
        assert_eq!(
            codeyard_db_identity::email::notification_email_address(&f.pool, unverified.id)
                .await
                .unwrap(),
            None
        );

        assert!(codeyard_db_identity::email::get_email_notifications(
            &f.pool,
            f.admin,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session("live")
        )
        .await
        .unwrap());
        codeyard_db_identity::email::set_email_notifications(
            &f.pool,
            f.admin,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session("live"),
            false,
        )
        .await
        .unwrap();
        assert!(!codeyard_db_identity::email::get_email_notifications(
            &f.pool,
            f.admin,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session("live")
        )
        .await
        .unwrap());
        assert_eq!(
            codeyard_db_identity::email::notification_email_address(&f.pool, f.admin)
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
            codeyard_db_repos::notifications::records::create_notification(
                &f.pool,
                codeyard_db_core::queries::notifications::records::NewNotification {
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
        // Signup review needs a finished setup and a live administrator session.
        codeyard_db_cluster::node_config::upsert_node_configuration(&f.pool, "public", true)
            .await
            .unwrap();
        let early = codeyard_db_identity::users::signup_requests::create_signup_request(
            &f.pool,
            "carol",
            Some("carol@example.com"),
            "hash",
        )
        .await
        .unwrap();
        codeyard_db_identity::users::signup_requests::reject_signup_request(
            &f.pool,
            early.id,
            (f.admin, "live".to_owned()),
        )
        .await
        .unwrap();
        assert!(
            codeyard_db_identity::email::claim_due_email(&f.pool, 5, 600)
                .await
                .unwrap()
                .is_none()
        );

        let input = settings("mail.example.com", None);
        codeyard_db_identity::email::save_email_settings(&f.pool, f.admin, "live", &input)
            .await
            .unwrap();
        let mail = notification(Some("Body"), Some("/x")).await.unwrap();
        notify_user(&f.pool, &mail).await;
        let later = codeyard_db_identity::users::signup_requests::create_signup_request(
            &f.pool,
            "dave",
            Some("dave@example.com"),
            "hash",
        )
        .await
        .unwrap();
        codeyard_db_identity::users::signup_requests::reject_signup_request(
            &f.pool,
            later.id,
            (f.admin, "live".to_owned()),
        )
        .await
        .unwrap();
        let first = codeyard_db_identity::email::claim_due_email(&f.pool, 5, 600)
            .await
            .unwrap()
            .unwrap();
        let second = codeyard_db_identity::email::claim_due_email(&f.pool, 5, 600)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (first.to_address.as_str(), first.subject.as_str()),
            ("admin@example.com", "Title")
        );
        assert_eq!(first.notification_id, Some(mail.id));
        assert!(
            codeyard_db_repos::notifications::records::notification_email_allowed(&f.pool, mail.id)
                .await
                .unwrap()
        );
        // The signup decision mail is queued with the decision once a mail server is set up.
        assert_eq!(second.to_address, "dave@example.com");
    }

    #[tokio::test]
    async fn the_outbox_claims_retries_gives_up_and_prunes() {
        let f = fixture().await;
        let DbPool::Sqlite(raw) = &f.pool else {
            unreachable!()
        };
        codeyard_db_identity::email::enqueue_email(&f.pool, "a@example.com", "One", "Body")
            .await
            .unwrap();

        // Two claims at once get the one message between them.
        let (left, right) = tokio::join!(
            codeyard_db_identity::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 600),
            codeyard_db_identity::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 600),
        );
        let claimed: Vec<_> = [left.unwrap(), right.unwrap()]
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].attempts, 1);
        // A claimed message is held, so it is not handed out again.
        assert!(
            codeyard_db_identity::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 600)
                .await
                .unwrap()
                .is_none()
        );

        // Each failure sets the next attempt; with no wait it is due again at once.
        let id = claimed[0].id;
        for attempts in 1..=outbox::MAX_ATTEMPTS {
            codeyard_db_identity::email::mark_email_failed(
                &f.pool,
                id,
                attempts,
                "refused",
                0,
                outbox::MAX_ATTEMPTS,
            )
            .await
            .unwrap();
            let next =
                codeyard_db_identity::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 600)
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
        codeyard_db_identity::email::enqueue_email(&f.pool, "b@example.com", "Two", "Body")
            .await
            .unwrap();
        let two = codeyard_db_identity::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 600)
            .await
            .unwrap()
            .unwrap();
        codeyard_db_identity::email::mark_email_failed(
            &f.pool,
            two.id,
            two.attempts,
            "later",
            3600,
            outbox::MAX_ATTEMPTS,
        )
        .await
        .unwrap();
        assert!(
            codeyard_db_identity::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 600)
                .await
                .unwrap()
                .is_none()
        );

        // Sent mail and given-up mail are pruned once old; waiting mail stays.
        codeyard_db_identity::email::enqueue_email(&f.pool, "c@example.com", "Three", "Body")
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
            codeyard_db_identity::email::remove_old_email(&f.pool, outbox::MAX_ATTEMPTS, 14)
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
        codeyard_db_identity::email::enqueue_email(&f.pool, "d@example.com", "Four", "Body")
            .await
            .unwrap();
        for attempt in 1..=outbox::MAX_ATTEMPTS {
            let claimed =
                codeyard_db_identity::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 0)
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(claimed.attempts, attempt);
        }
        assert!(
            codeyard_db_identity::email::claim_due_email(&f.pool, outbox::MAX_ATTEMPTS, 0)
                .await
                .unwrap()
                .is_none()
        );
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

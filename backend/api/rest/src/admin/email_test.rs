use codeyard_database::DbPool;
use codeyard_db_identity::mail_config as config;
use codeyard_notifications::email::send;
use codeyard_security::secrets::SecretEncryptor;

#[derive(Debug, thiserror::Error)]
pub enum TestMailError {
    #[error("A live unrestricted administrator session is required")]
    Authority(codeyard_identity::sessions::AccountAuthorityError),
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
    let (config, _) = config::load(pool, secrets)
        .await
        .map_err(TestMailError::Delivery)?
        .ok_or_else(|| {
            TestMailError::Delivery("Email is not set up. Save the settings first.".into())
        })?;
    let admin = codeyard_identity::sessions::administrator(pool, Some(session))
        .await
        .map_err(TestMailError::Authority)?;
    let to = admin
        .email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .ok_or(TestMailError::NoAddress)?;
    let mail = codeyard_types::mail_messages::test_message();
    send::send(&config, to, &mail.subject, &mail.body)
        .await
        .map_err(TestMailError::Delivery)?;
    Ok(to.to_owned())
}

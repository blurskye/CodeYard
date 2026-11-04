//! The mail server settings: read from the database row or from CODEYARD_SMTP_* variables.
use codeyard_support::secrets::SecretEncryptor;

use codeyard_db_core::models::EmailSettings;
use codeyard_db_core::DbPool;
use lettre::message::Mailbox;
use std::str::FromStr;

fn unavailable(error: impl std::fmt::Display) -> String {
    tracing::warn!(%error, "could not read email settings");
    "The email settings are unavailable.".to_owned()
}

fn from_row(row: EmailSettings, password: Option<String>) -> Result<SmtpConfig, String> {
    Ok(SmtpConfig {
        security: Security::parse(&row.security).ok_or_else(|| unavailable("bad security"))?,
        port: u16::try_from(row.port).map_err(unavailable)?,
        host: row.host,
        username: row.username,
        password,
        from_name: row.from_name,
        from_address: row.from_address,
        reply_to: row.reply_to,
    })
}

/// The settings in use: the saved ones, else the environment's. The error text is safe to show
/// an administrator.
pub async fn load(
    pool: &DbPool,
    secrets: Option<&SecretEncryptor>,
) -> Result<Option<(SmtpConfig, Source)>, String> {
    let Some(row) = crate::email::get_email_settings(pool)
        .await
        .map_err(unavailable)?
    else {
        return Ok(from_env()?.map(|config| (config, Source::Environment)));
    };
    let password = match (&row.encrypted_password, &row.password_nonce) {
        _ if row.username.is_empty() => None,
        (Some(ciphertext), Some(nonce)) => {
            let secrets =
                secrets.ok_or("SECRETS_MASTER_KEY is needed to use the saved mail password.")?;
            let bytes = secrets
                .decrypt(ciphertext, nonce)
                .map_err(|_| "The saved mail password cannot be opened with SECRETS_MASTER_KEY.")?;
            Some(String::from_utf8(bytes).map_err(unavailable)?)
        }
        _ => None,
    };
    Ok(Some((from_row(row, password)?, Source::Database)))
}

/// The settings in use without opening the password, for the admin page. The flag says whether
/// a password is set.
pub async fn describe(pool: &DbPool) -> Result<Option<(SmtpConfig, Source, bool)>, String> {
    match crate::email::get_email_settings(pool)
        .await
        .map_err(unavailable)?
    {
        Some(row) => {
            let password_set = !row.username.is_empty() && row.encrypted_password.is_some();
            Ok(Some((from_row(row, None)?, Source::Database, password_set)))
        }
        None => Ok(from_env()?.map(|config| {
            let password_set = config.password.is_some();
            (config, Source::Environment, password_set)
        })),
    }
}

/// Whether a mail server is set up, without opening the password.
pub async fn is_configured(pool: &DbPool) -> bool {
    match crate::email::get_email_settings(pool).await {
        Ok(Some(_)) => true,
        Ok(None) => environment_configured(),
        Err(error) => {
            tracing::warn!(%error, "could not read email settings");
            false
        }
    }
}

use codeyard_model::mail_config::Security;

use codeyard_model::mail_config::SmtpConfig;

use codeyard_model::mail_config::Source;

use codeyard_model::mail_config::environment_configured;

use codeyard_model::mail_config::from_env;

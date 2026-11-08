//! Links mailed to a user: one confirms an address, one lets a forgotten password be reset. The
//! mail carries a random token, shown as a code to paste and, when PUBLIC_URL is set, in a link.
//! Only its hash is stored. A token works once, until it expires.

use codeyard_types::http_config::public_origin;

use codeyard_database::models::User;
use codeyard_database::DbPool;
use codeyard_db_identity::auth_admission::AdmissionError;
use codeyard_security::token_hash::token_hash as hash;
use tracing::warn;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Verify,
    Reset,
}

impl Purpose {
    pub fn name(self) -> &'static str {
        match self {
            Purpose::Verify => "verify",
            Purpose::Reset => "reset",
        }
    }

    pub fn lifetime_seconds(self) -> i64 {
        match self {
            Purpose::Verify => 24 * 60 * 60,
            Purpose::Reset => 60 * 60,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum LinkError {
    /// The site has no mail server.
    NotConfigured,
    NoAddress,
    Limited,
    Unavailable,
}

impl From<AdmissionError> for LinkError {
    fn from(error: AdmissionError) -> Self {
        match error {
            AdmissionError::Limited => LinkError::Limited,
            AdmissionError::Unavailable => LinkError::Unavailable,
        }
    }
}

fn unavailable(error: anyhow::Error) -> LinkError {
    warn!(%error, "could not use an email link");
    LinkError::Unavailable
}

async fn queue_link(
    pool: &DbPool,
    purpose: Purpose,
    user: &User,
    email: &str,
) -> Result<(), LinkError> {
    let token = codeyard_security::crypto::random_hex();
    let origin = public_origin();
    let mail = match purpose {
        Purpose::Verify => {
            codeyard_types::mail_messages::verify_email(&origin, &user.username, &token)
        }
        Purpose::Reset => {
            codeyard_types::mail_messages::password_reset(&origin, &user.username, &token)
        }
    };
    // The token and its mail are saved together. A mail sent less than a minute ago still holds
    // the live token, and then nothing is saved; the answer stays the same.
    codeyard_db_identity::email::save_email_token(
        pool,
        &codeyard_db_identity::email::EmailLink {
            user_id: user.id,
            purpose: purpose.name(),
            email,
            token_hash: &hash(&token),
            ttl_seconds: purpose.lifetime_seconds(),
            subject: &mail.subject,
            body_text: &mail.body,
        },
    )
    .await
    .map_err(unavailable)?;
    Ok(())
}

/// Mails the user a code that confirms their current address. A new mail replaces the last one,
/// unless that one is less than a minute old.
pub async fn send_verification(pool: &DbPool, user: &User) -> Result<(), LinkError> {
    let email = user
        .email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .ok_or(LinkError::NoAddress)?;
    if !codeyard_db_identity::mail_config::is_configured(pool).await {
        return Err(LinkError::NotConfigured);
    }
    codeyard_db_identity::auth_admission::admit_mail(pool, None, &format!("user:{}", user.id))
        .await?;
    queue_link(pool, Purpose::Verify, user, email).await
}

/// Mails each of the user's new secondary addresses its own code, so adding a batch does not
/// invalidate earlier mail. Each address keeps its own resend interval. The batch is charged
/// once against the mail budget. NoAddress when none of the addresses awaits a code.
pub async fn send_secondary_verifications(
    pool: &DbPool,
    user: &User,
    emails: &[String],
) -> Result<(), LinkError> {
    let addresses = codeyard_db_identity::users::emails::list_emails(pool, user.id)
        .await
        .map_err(unavailable)?;
    let pending: Vec<&str> = emails
        .iter()
        .map(String::as_str)
        .filter(|&email| {
            addresses
                .iter()
                .any(|address| address.email.eq_ignore_ascii_case(email) && !address.verified)
        })
        .collect();
    if pending.is_empty() {
        return Err(LinkError::NoAddress);
    }
    if !codeyard_db_identity::mail_config::is_configured(pool).await {
        return Err(LinkError::NotConfigured);
    }
    codeyard_db_identity::auth_admission::admit_mail(pool, None, &format!("user:{}", user.id))
        .await?;
    for email in pending {
        let token = codeyard_security::crypto::random_hex();
        let mail =
            codeyard_types::mail_messages::verify_email(&public_origin(), &user.username, &token);
        codeyard_db_identity::users::emails::save_verification(
            pool,
            user.id,
            email,
            &hash(&token),
            &mail.subject,
            &mail.body,
        )
        .await
        .map_err(unavailable)?;
    }
    Ok(())
}

/// Mails a password reset code to the account that has this verified address. Nothing tells the
/// caller whether such an account exists, so the answer is the same for every address.
pub async fn send_password_reset(
    pool: &DbPool,
    client: Option<&str>,
    address: &str,
) -> Result<(), LinkError> {
    let address = address.trim();
    if !codeyard_db_identity::mail_config::is_configured(pool).await {
        return Err(LinkError::NotConfigured);
    }
    let key = hash(&address.to_lowercase());
    codeyard_db_identity::auth_admission::admit_mail(pool, client, &key).await?;
    let user = codeyard_db_identity::users::accounts::get_user_by_email(pool, address)
        .await
        .map_err(unavailable)?
        .filter(|user| user.email_verified && user.password_hash.is_some() && !user.is_banned);
    let Some(user) = user else {
        return Ok(());
    };
    // The mail goes to the address on the account, which may differ in case from the one typed.
    let Some(stored) = user.email.as_deref() else {
        return Ok(());
    };
    queue_link(pool, Purpose::Reset, &user, stored).await
}

/// Whether the link can still be used, without using it.
pub async fn is_valid(pool: &DbPool, purpose: Purpose, token: &str) -> Result<bool, LinkError> {
    let hash = hash(token);
    let valid = codeyard_db_identity::email::email_token_is_valid(pool, purpose.name(), &hash)
        .await
        .map_err(unavailable)?;
    if !valid && purpose == Purpose::Verify {
        return codeyard_db_identity::users::emails::verification_valid(pool, &hash)
            .await
            .map_err(unavailable);
    }
    Ok(valid)
}

pub(crate) async fn password_reset_account(
    pool: &DbPool,
    token: &str,
) -> Result<Option<User>, LinkError> {
    codeyard_db_identity::users::transitions::password_reset_account(pool, &hash(token))
        .await
        .map_err(unavailable)
}

/// Uses up a reset link and sets the account's new password hash, in one transaction. Returns the
/// account the link was for, or None when the link is no longer usable.
pub async fn use_reset_link(
    pool: &DbPool,
    token: &str,
    password_hash: &str,
    expected: &User,
) -> Result<Option<i64>, LinkError> {
    let Some(expected_hash) = expected.password_hash.as_deref() else {
        return Ok(None);
    };
    codeyard_db_identity::users::transitions::reset_password_with_link(
        pool,
        &hash(token),
        password_hash,
        expected_hash,
        expected.must_change_password,
    )
    .await
    .map_err(unavailable)
}

/// Uses up a verification link and marks the address verified. False when the link is unknown,
/// expired, already used, or the user has changed their address since.
pub async fn verify_address(pool: &DbPool, token: &str) -> Result<bool, LinkError> {
    let hash = hash(token);
    match codeyard_db_identity::email::verify_email_token(pool, &hash)
        .await
        .map_err(unavailable)?
    {
        Some(confirmed) => Ok(confirmed),
        None => codeyard_db_identity::users::emails::verify_email(pool, &hash)
            .await
            .map_err(unavailable),
    }
}

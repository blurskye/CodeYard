//! Links mailed to a user: one confirms an address, one lets a forgotten password be reset. The
//! mail carries a random token, shown as a code to paste and, when PUBLIC_URL is set, in a link.
//! Only its hash is stored. A token works once, until it expires.
use super::{config, messages, outbox};
use crate::backend::api::http::config::public_origin;
use crate::backend::db::{self, models::User, DbPool};
use crate::backend::identity::admission::{self, AdmissionError};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};
use tracing::warn;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Verify,
    Reset,
}

impl Purpose {
    fn name(self) -> &'static str {
        match self {
            Purpose::Verify => "verify",
            Purpose::Reset => "reset",
        }
    }

    fn lifetime_seconds(self) -> i64 {
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

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
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
    let token = new_token();
    let saved = db::email::save_email_token(
        pool,
        user.id,
        purpose.name(),
        email,
        &hash(&token),
        purpose.lifetime_seconds(),
    )
    .await
    .map_err(unavailable)?;
    // The mail sent less than a minute ago still holds the live token; the answer stays the same.
    if !saved {
        return Ok(());
    }
    let origin = public_origin();
    let mail = match purpose {
        Purpose::Verify => messages::verify_email(&origin, &user.username, &token),
        Purpose::Reset => messages::password_reset(&origin, &user.username, &token),
    };
    outbox::enqueue(pool, email, mail).await;
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
    if !config::is_configured(pool).await {
        return Err(LinkError::NotConfigured);
    }
    admission::admit_mail(pool, None, &format!("user:{}", user.id)).await?;
    queue_link(pool, Purpose::Verify, user, email).await
}

/// Mails a password reset code to the account that has this verified address. Nothing tells the
/// caller whether such an account exists, so the answer is the same for every address.
pub async fn send_password_reset(
    pool: &DbPool,
    client: Option<&str>,
    address: &str,
) -> Result<(), LinkError> {
    let address = address.trim();
    if !config::is_configured(pool).await {
        return Err(LinkError::NotConfigured);
    }
    let key = hex::encode(Sha256::digest(address.to_lowercase().as_bytes()));
    admission::admit_mail(pool, client, &key).await?;
    let user = db::users::accounts::get_user_by_email(pool, address)
        .await
        .map_err(unavailable)?
        .filter(|user| user.email_verified && user.password_hash.is_some() && !user.is_banned);
    match user {
        Some(user) => queue_link(pool, Purpose::Reset, &user, address).await,
        None => Ok(()),
    }
}

/// Whether the link can still be used, without using it.
pub async fn is_valid(pool: &DbPool, purpose: Purpose, token: &str) -> Result<bool, LinkError> {
    db::email::email_token_is_valid(pool, purpose.name(), &hash(token))
        .await
        .map_err(unavailable)
}

/// Uses up the link. Returns the user it was for and the address it was sent to.
pub async fn take(
    pool: &DbPool,
    purpose: Purpose,
    token: &str,
) -> Result<Option<(i64, String)>, LinkError> {
    db::email::take_email_token(pool, purpose.name(), &hash(token))
        .await
        .map_err(unavailable)
}

/// Uses up a verification link and marks the address verified. False when the link is unknown,
/// expired, already used, or the user has changed their address since.
pub async fn verify_address(pool: &DbPool, token: &str) -> Result<bool, LinkError> {
    let Some((user_id, email)) = take(pool, Purpose::Verify, token).await? else {
        return Ok(false);
    };
    db::email::confirm_email(pool, user_id, &email)
        .await
        .map_err(unavailable)
}

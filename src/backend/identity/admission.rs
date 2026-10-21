//! Failed-login limits per client address and per account, shared by every node. Storage that
//! cannot answer denies the attempt.

use crate::backend::db::{self, DbPool};
use sha2::{Digest, Sha256};
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub enum AdmissionError {
    Limited,
    Unavailable,
}

/// The budgets one admitted attempt was counted against, so a right password can give it back.
#[derive(Debug, PartialEq, Eq)]
pub struct Attempt {
    client: Option<String>,
    account: String,
}

/// Counts an attempt before its credentials are checked, against `client` (the request's address,
/// when known) and the account: the user, or the hash of the identity when there is no such user.
pub async fn admit_login(
    pool: &DbPool,
    client: Option<&str>,
    user_id: Option<i64>,
    identity: &str,
) -> Result<Attempt, AdmissionError> {
    let account = match user_id {
        Some(id) => format!("user:{id}"),
        None => format!(
            "unknown:{}",
            hex::encode(Sha256::digest(identity.trim().to_lowercase().as_bytes()))
        ),
    };
    admit(pool, client.map(|client| format!("ip:{client}")), account).await
}

/// Counts a request for a mailed link against `client` and `key` (a user, or the hash of an
/// address). Every request counts, so the answer says nothing about whether the key exists.
/// Mail requests have their own budget per client address, so they cannot use up the one for
/// signing in.
pub async fn admit_mail(
    pool: &DbPool,
    client: Option<&str>,
    key: &str,
) -> Result<(), AdmissionError> {
    let client = client.map(|client| format!("mail-ip:{client}"));
    admit(pool, client, format!("mail:{key}")).await.map(drop)
}

async fn admit(
    pool: &DbPool,
    client: Option<String>,
    account: String,
) -> Result<Attempt, AdmissionError> {
    let admitted = tokio::time::timeout(
        Duration::from_secs(5),
        db::users::admission::admit_password_attempt(pool, client.as_deref(), &account),
    )
    .await
    .map_err(|_| AdmissionError::Unavailable)?
    .map_err(|_| AdmissionError::Unavailable)?;
    if admitted {
        Ok(Attempt { client, account })
    } else {
        Err(AdmissionError::Limited)
    }
}

/// Gives the attempt back once the credentials checked out, so only failures count. If storage
/// cannot take it back, the attempt stays counted.
pub async fn forgive_login(pool: &DbPool, attempt: &Attempt) {
    let forgiven = tokio::time::timeout(
        Duration::from_secs(5),
        db::users::admission::forgive_password_attempt(
            pool,
            attempt.client.as_deref(),
            &attempt.account,
        ),
    )
    .await;
    if !matches!(forgiven, Ok(Ok(()))) {
        tracing::warn!("could not give back a successful sign-in attempt");
    }
}

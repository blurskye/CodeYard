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
    let client = client.map(|client| format!("ip:{client}"));
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

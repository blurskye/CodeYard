//! Git-only token creation. Plaintext is returned once; only its hash is persisted.

use crate::backend::db::{models::PersonalAccessToken, users::tokens, DbPool};
use chrono::{DateTime, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};

pub struct TokenInput<'a> {
    pub name: &'a str,
    pub scope: &'a str,
    pub expires_at: Option<DateTime<Utc>>,
}

pub struct CreatedToken {
    pub token: PersonalAccessToken,
    pub value: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenError {
    #[error("Use a name of 1-100 characters, scope read or write, and a future expiry")]
    Invalid,
    #[error("Account is unavailable")]
    Denied,
    #[error("Token not found")]
    Missing,
    #[error("Token storage is unavailable")]
    Unavailable,
}

pub fn token_hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

pub fn validate(input: &TokenInput<'_>, now: DateTime<Utc>) -> Result<(), TokenError> {
    if input.name.trim().is_empty()
        || input.name.trim().len() > 100
        || !matches!(input.scope, "read" | "write")
        || input.expires_at.is_some_and(|expiry| expiry <= now)
    {
        return Err(TokenError::Invalid);
    }
    Ok(())
}

/// Creates a token for `actor`. `session_id` must still be a live session of `actor` when
/// the row is inserted; a password change that revoked it makes this fail with `Denied`.
pub async fn create(
    pool: &DbPool,
    actor: i64,
    session_id: Option<&str>,
    input: TokenInput<'_>,
) -> Result<CreatedToken, TokenError> {
    validate(&input, Utc::now())?;
    let session_id = session_id.ok_or(TokenError::Denied)?;
    let mut random = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut random);
    let value = hex::encode(random);
    let token = tokens::create_token(
        pool,
        actor,
        session_id,
        input.name.trim(),
        &token_hash(&value),
        input.scope,
        input.expires_at,
    )
    .await
    .map_err(|_| TokenError::Unavailable)?
    .ok_or(TokenError::Denied)?;
    Ok(CreatedToken { token, value })
}

pub async fn delete(pool: &DbPool, actor: i64, id: i64) -> Result<(), TokenError> {
    if tokens::delete_token(pool, actor, id)
        .await
        .map_err(|_| TokenError::Unavailable)?
    {
        Ok(())
    } else {
        Err(TokenError::Missing)
    }
}

//! Personal token lifecycle (only hashes persist) and restricted activity-feed credentials.

use crate::backend::db::{models::PersonalAccessToken, users::tokens, DbPool};
use chrono::{DateTime, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};

pub(crate) const TOKEN_BYTES: usize = 32;
pub(crate) const HEX_TOKEN_CHARS: usize = TOKEN_BYTES * 2;
pub(crate) const URL_TOKEN_CHARS: usize = (TOKEN_BYTES * 8 + 5) / 6;
pub(crate) const PAT_PREFIX: &str = "cy_pat_";
pub(crate) const FGPAT_PREFIX: &str = "cy_fgpat_";
pub(crate) const ACTIONS_PREFIX: &str = "ghs_";
pub(crate) const INSTALLATION_PREFIX: &str = "ghs_";

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

pub(super) fn random_token() -> String {
    let mut random = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut random);
    hex::encode(random)
}

/// Installation credentials use the hex format; Actions uses the URL-safe format with
/// the same public prefix. Scanners must recognize both complete values.
pub(crate) fn installation_token() -> String {
    format!("{INSTALLATION_PREFIX}{}", random_token())
}

/// A feed credential has only read-only event access and remains bound to this API token's
/// lifetime. Deleting the API token cascades to the feed credential; expiry is checked on reads.
pub async fn activity_feed_token(pool: &DbPool, token_id: i64) -> Result<String, TokenError> {
    tokens::token_user(pool, token_id, false)
        .await
        .map_err(|_| TokenError::Unavailable)?
        .ok_or(TokenError::Denied)?;
    // Feeds carry an account identity, not repository-specific integration bounds.
    if super::github_apps::authority(pool, token_id)
        .await
        .map_err(|_| TokenError::Unavailable)?
        .is_some()
    {
        return Err(TokenError::Denied);
    }
    crate::backend::db::events::activity_feed_token(
        pool,
        token_id,
        &format!("feed_{}", random_token()),
    )
    .await
    .map_err(|_| TokenError::Unavailable)?
    .ok_or(TokenError::Denied)
}
pub async fn activity_feed_user(
    pool: &DbPool,
    value: &str,
) -> Result<crate::backend::db::models::User, TokenError> {
    if value.len() != 69 || !value.starts_with("feed_") {
        return Err(TokenError::Denied);
    }
    let id = crate::backend::db::events::activity_feed_token_id(pool, value)
        .await
        .map_err(|_| TokenError::Unavailable)?
        .ok_or(TokenError::Denied)?;
    let user = tokens::token_user(pool, id, false)
        .await
        .map_err(|_| TokenError::Unavailable)?
        .ok_or(TokenError::Denied)?;
    if super::github_apps::authority(pool, id)
        .await
        .map_err(|_| TokenError::Unavailable)?
        .is_some()
    {
        return Err(TokenError::Denied);
    }
    Ok(user)
}

pub fn token_hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

pub fn validate(input: &TokenInput<'_>, now: DateTime<Utc>) -> Result<(), TokenError> {
    if input.name.trim().is_empty()
        || input.name.trim().len() > 100
        || input.name.contains('\0')
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
    let value = format!("{PAT_PREFIX}{}", random_token());
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

pub async fn delete(
    pool: &DbPool,
    actor: i64,
    session_id: Option<&str>,
    id: i64,
) -> Result<(), TokenError> {
    let session_id = session_id.ok_or(TokenError::Denied)?;
    if tokens::delete_token(pool, actor, session_id, id)
        .await
        .map_err(|_| TokenError::Unavailable)?
    {
        Ok(())
    } else {
        Err(TokenError::Missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_name_rejects_nul() {
        let input = TokenInput {
            name: "ci\0token",
            scope: "read",
            expires_at: None,
        };
        assert_eq!(validate(&input, Utc::now()), Err(TokenError::Invalid));
    }
}

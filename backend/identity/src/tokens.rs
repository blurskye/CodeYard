//! Personal token lifecycle (only hashes persist) and restricted activity-feed credentials.

use chrono::{DateTime, Utc};
use codeyard_database::models::PersonalAccessToken;
use codeyard_database::DbPool;

use codeyard_types::tokens::INSTALLATION_PREFIX;
use codeyard_types::tokens::PAT_PREFIX;

pub struct TokenInput<'a> {
    pub name: &'a str,
    pub scope: &'a str,
    pub expires_at: Option<DateTime<Utc>>,
}

pub struct CreatedToken {
    pub token: PersonalAccessToken,
    pub value: String,
}

pub struct ActivityFeedUser {
    pub user: codeyard_database::models::User,
    pub token: i64,
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

/// Installation credentials use the hex format; Actions uses the URL-safe format with
/// the same public prefix. Scanners must recognize both complete values.
pub fn installation_token(
    cell: codeyard_types::cell_ids::Prefix,
) -> Result<String, codeyard_types::cell_ids::Invalid> {
    cell.token(
        INSTALLATION_PREFIX,
        &codeyard_security::crypto::random_hex(),
    )
}

/// A feed credential has only read-only event access and remains bound to this API token's
/// lifetime. Deleting the API token cascades to the feed credential; expiry is checked on reads.
pub async fn activity_feed_token(pool: &DbPool, token_id: i64) -> Result<String, TokenError> {
    codeyard_db_identity::users::tokens::token_user(pool, token_id, false)
        .await
        .map_err(|_| TokenError::Unavailable)?
        .ok_or(TokenError::Denied)?;
    // Feeds carry an account identity, not repository-specific integration bounds.
    if crate::github_apps::authority(pool, token_id)
        .await
        .map_err(|_| TokenError::Unavailable)?
        .is_some()
    {
        return Err(TokenError::Denied);
    }
    codeyard_db_repos::events::activity_feed_token(
        pool,
        token_id,
        &format!("feed_{}", codeyard_security::crypto::random_hex()),
    )
    .await
    .map_err(|_| TokenError::Unavailable)?
    .ok_or(TokenError::Denied)
}
pub async fn activity_feed_user(
    pool: &DbPool,
    value: &str,
) -> Result<ActivityFeedUser, TokenError> {
    if value.len() != 69 || !value.starts_with("feed_") {
        return Err(TokenError::Denied);
    }
    let id = codeyard_db_repos::events::activity_feed_token_id(pool, value)
        .await
        .map_err(|_| TokenError::Unavailable)?
        .ok_or(TokenError::Denied)?;
    let user = codeyard_db_identity::users::tokens::token_user(pool, id, false)
        .await
        .map_err(|_| TokenError::Unavailable)?
        .ok_or(TokenError::Denied)?;
    if crate::github_apps::authority(pool, id)
        .await
        .map_err(|_| TokenError::Unavailable)?
        .is_some()
    {
        return Err(TokenError::Denied);
    }
    Ok(ActivityFeedUser { user, token: id })
}

/// Recheck a retained token's account, organization policy and effective repository grant.
pub async fn repository_scope(
    pool: &DbPool,
    token: i64,
    actor: i64,
    repo: i64,
    permission: &str,
    write: bool,
) -> anyhow::Result<bool> {
    if !codeyard_db_identity::users::tokens::token_user(pool, token, write)
        .await?
        .is_some_and(|user| user.id == actor)
        || codeyard_db_identity::fine_grained_tokens::classic_refused_repository(pool, token, repo)
            .await?
    {
        return Ok(false);
    }
    if let Some(authority) = crate::github_apps::authority(pool, token).await? {
        return authority.allows(pool, actor, repo, permission, write).await;
    }
    if let Some(job) = codeyard_db_actions::ci_tokens::get(pool, token).await? {
        return Ok(job.repo_id == repo
            && codeyard_types::github_apps::allows(
                &serde_json::from_str(&job.permissions)?,
                permission,
                write,
            ));
    }
    if let Some(oauth) = codeyard_db_identity::oauth_apps::token(pool, token).await? {
        if crate::oauth_apps::client(pool, &oauth.client_id)
            .await?
            .is_none()
        {
            return Ok(false);
        }
        return Ok(codeyard_types::oauth_apps::has_scope(&oauth.scopes, "repo")
            || (!write
                && codeyard_db_repos::repositories::records::get_repository_by_id(pool, repo)
                    .await?
                    .is_some_and(|repo| !repo.is_private)));
    }
    Ok(true)
}

/// The stored form of a personal access token name, classic or fine-grained: trimmed,
/// 1-100 characters and without control characters.
pub fn valid_name(name: &str) -> Option<&str> {
    let name = name.trim();
    (!name.is_empty() && name.chars().count() <= 100 && !name.chars().any(char::is_control))
        .then_some(name)
}

pub fn validate(input: &TokenInput<'_>, now: DateTime<Utc>) -> Result<(), TokenError> {
    if valid_name(input.name).is_none()
        || !matches!(input.scope, "read" | "write")
        || !codeyard_database::queries::users::tokens::supported_expiry(input.expires_at)
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
    let value = pool
        .cell_prefix()
        .token(PAT_PREFIX, &codeyard_security::crypto::random_hex())
        .map_err(|_| TokenError::Unavailable)?;
    let token = codeyard_db_identity::users::tokens::create_token(
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
    if codeyard_db_identity::users::tokens::delete_token(pool, actor, session_id, id)
        .await
        .map_err(|_| TokenError::Unavailable)?
    {
        Ok(())
    } else {
        Err(TokenError::Missing)
    }
}

use codeyard_security::token_hash::token_hash;

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

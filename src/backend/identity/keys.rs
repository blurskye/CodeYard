//! SSH key management shared by browser forms, JSON and operator commands.

use codeyard_db_core::models::SshKey;
use codeyard_db_core::DbPool;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("Key name must be 1-100 characters")]
    InvalidName,
    #[error("Invalid SSH public key format")]
    InvalidKey,
    #[error("This SSH public key is already registered")]
    AlreadyUsed,
    #[error("SSH keys cannot be changed while the account is restricted")]
    AccountRestricted,
    #[error("SSH key enrollment requires a live session")]
    InvalidSession,
    #[error("SSH key not found")]
    Missing,
    #[error("SSH key enrollment is temporarily unavailable")]
    Unavailable,
}

pub fn validate_name(name: &str) -> Result<(), KeyError> {
    if name.is_empty() || name.chars().count() > 100 || name.contains('\0') {
        Err(KeyError::InvalidName)
    } else {
        Ok(())
    }
}

pub async fn enroll(
    pool: &DbPool,
    actor: i64,
    session_id: Option<&str>,
    name: &str,
    public_key: &str,
) -> Result<SshKey, KeyError> {
    validate_name(name)?;
    let session_id = session_id.ok_or(KeyError::InvalidSession)?;
    codeyard_db_identity::users::ssh_keys::add_ssh_key_for_session(
        pool, actor, session_id, name, public_key,
    )
    .await
    .map_err(|error| {
        match error.downcast_ref::<codeyard_model::identity::SshKeyWriteRefusal>() {
            Some(codeyard_model::identity::SshKeyWriteRefusal::InvalidKey) => KeyError::InvalidKey,
            Some(codeyard_model::identity::SshKeyWriteRefusal::AlreadyUsed) => {
                KeyError::AlreadyUsed
            }
            Some(codeyard_model::identity::SshKeyWriteRefusal::InvalidSession) => {
                KeyError::InvalidSession
            }
            Some(codeyard_model::identity::SshKeyWriteRefusal::AccountRestricted) => {
                KeyError::AccountRestricted
            }
            None => KeyError::Unavailable,
        }
    })
}

pub async fn delete(
    pool: &DbPool,
    actor: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    key: i64,
) -> Result<(), KeyError> {
    let deleted =
        codeyard_db_identity::users::ssh_keys::delete_user_ssh_key(pool, actor, credential, key)
            .await
            .map_err(|error| {
                match error.downcast_ref::<codeyard_model::identity::SshKeyWriteRefusal>() {
                    Some(codeyard_model::identity::SshKeyWriteRefusal::AccountRestricted) => {
                        KeyError::AccountRestricted
                    }
                    Some(codeyard_model::identity::SshKeyWriteRefusal::InvalidSession) => {
                        KeyError::InvalidSession
                    }
                    _ => KeyError::Unavailable,
                }
            })?;
    if deleted {
        Ok(())
    } else {
        Err(KeyError::Missing)
    }
}

fn signing_error(error: anyhow::Error) -> KeyError {
    if error.is::<codeyard_support::credentials::AccountRestricted>() {
        return KeyError::AccountRestricted;
    }
    match error.downcast_ref::<codeyard_model::identity::SshKeyWriteRefusal>() {
        Some(codeyard_model::identity::SshKeyWriteRefusal::InvalidKey) => KeyError::InvalidKey,
        Some(codeyard_model::identity::SshKeyWriteRefusal::AlreadyUsed) => KeyError::AlreadyUsed,
        _ => KeyError::Unavailable,
    }
}

pub async fn enroll_signing(
    pool: &DbPool,
    actor: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    title: &str,
    public_key: &str,
) -> Result<codeyard_db_identity::users::signing_keys::SigningKey, KeyError> {
    validate_name(title)?;
    codeyard_db_identity::users::signing_keys::add_signing_key(
        pool, actor, credential, title, public_key,
    )
    .await
    .map_err(signing_error)
}

pub async fn delete_signing(
    pool: &DbPool,
    actor: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    id: i64,
) -> Result<(), KeyError> {
    if codeyard_db_identity::users::signing_keys::delete_signing_key(pool, actor, credential, id)
        .await
        .map_err(signing_error)?
    {
        Ok(())
    } else {
        Err(KeyError::Missing)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_reject_nul() {
        assert!(crate::backend::identity::keys::validate_name("ci\0key").is_err());
        assert!(crate::backend::identity::keys::validate_name("ci key").is_ok());
    }
}

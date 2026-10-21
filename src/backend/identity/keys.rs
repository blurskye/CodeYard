//! SSH key management shared by browser forms, JSON and operator commands.

use crate::backend::db::{models::SshKey, users::ssh_keys, DbPool};

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
    ssh_keys::add_ssh_key_for_session(pool, actor, session_id, name, public_key)
        .await
        .map_err(
            |error| match error.downcast_ref::<ssh_keys::SshKeyWriteRefusal>() {
                Some(ssh_keys::SshKeyWriteRefusal::InvalidKey) => KeyError::InvalidKey,
                Some(ssh_keys::SshKeyWriteRefusal::AlreadyUsed) => KeyError::AlreadyUsed,
                Some(ssh_keys::SshKeyWriteRefusal::InvalidSession) => KeyError::InvalidSession,
                Some(ssh_keys::SshKeyWriteRefusal::AccountRestricted) => {
                    KeyError::AccountRestricted
                }
                None => KeyError::Unavailable,
            },
        )
}

pub async fn delete(pool: &DbPool, actor: i64, key: i64) -> Result<(), KeyError> {
    let deleted = ssh_keys::delete_user_ssh_key(pool, actor, key)
        .await
        .map_err(
            |error| match error.downcast_ref::<ssh_keys::SshKeyWriteRefusal>() {
                Some(ssh_keys::SshKeyWriteRefusal::AccountRestricted) => {
                    KeyError::AccountRestricted
                }
                _ => KeyError::Unavailable,
            },
        )?;
    if deleted {
        Ok(())
    } else {
        Err(KeyError::Missing)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_reject_nul() {
        assert!(super::validate_name("ci\0key").is_err());
        assert!(super::validate_name("ci key").is_ok());
    }
}

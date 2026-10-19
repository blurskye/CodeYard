//! Shared named operation surface; native transactions stay inside db.
use crate::backend::crypto::{SecretEncryptor, SecretError};
use crate::backend::db::models::RepoSecretMeta;
use crate::backend::db::DbPool;

pub const MAX_SECRET_NAME_BYTES: usize = 255;
pub const MAX_SECRET_VALUE_BYTES: usize = 48 * 1024;
/// Most repository secrets one CI claim carries.
pub const MAX_REPOSITORY_SECRETS: usize = 100;
/// Names and values of one claim together. A claim is one RPC reply of at most 8 MiB, and
/// JSON escapes a control byte as six bytes, so 1 MiB of secret text stays inside the frame.
pub const MAX_REPOSITORY_SECRETS_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("Secret database unavailable")]
    Database(#[from] sqlx::Error),
    #[error("Secret encryption unavailable")]
    Encryption(#[from] SecretError),
    #[error("secret_authority_denied")]
    Denied,
    #[error("Secret authority unavailable")]
    Authority(#[source] anyhow::Error),
    #[error("Secret not found")]
    NotFound,
    #[error("Repository secret name is invalid")]
    InvalidName,
    #[error("Repository secret name uses a reserved prefix")]
    ReservedName,
    #[error("Repository secret value contains NUL")]
    ValueContainsNul,
    #[error("Repository secret value exceeds 48 KiB")]
    ValueTooLarge,
    #[error("Repository has too many secrets for CI")]
    TooManySecrets,
    #[error("Repository secrets exceed 1 MiB in total")]
    TotalTooLarge,
}

impl SecretsError {
    pub(crate) fn authority(error: anyhow::Error) -> Self {
        // Anyhow downcasts also recognize contextual errors; retain typed denial
        // and driver sources without displaying SQL, keys or helper context.
        let error = match error.downcast::<Self>() {
            Ok(error) => return error,
            Err(error) => error,
        };
        if matches!(
            error.downcast_ref::<crate::backend::db::discussions::DiscussionWriteRefusal>(),
            Some(crate::backend::db::discussions::DiscussionWriteRefusal::Denied)
        ) {
            return Self::Denied;
        }
        match error.downcast::<sqlx::Error>() {
            Ok(error) => Self::Database(error),
            Err(error) => Self::Authority(error),
        }
    }
}

pub fn validate_repository_secret(name: &str, value: &str) -> Result<(), SecretsError> {
    let mut bytes = name.bytes();
    let first = bytes.next().ok_or(SecretsError::InvalidName)?;
    if name.len() > MAX_SECRET_NAME_BYTES
        || !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(SecretsError::InvalidName);
    }
    if name
        .get(.."GITHUB_".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("GITHUB_"))
    {
        return Err(SecretsError::ReservedName);
    }
    if value.as_bytes().contains(&0) {
        return Err(SecretsError::ValueContainsNul);
    }
    if value.len() > MAX_SECRET_VALUE_BYTES {
        return Err(SecretsError::ValueTooLarge);
    }
    Ok(())
}

/// Checks the whole set that one CI claim carries: each secret, then the count and the
/// total size.
pub fn validate_repository_secrets(secrets: &[(String, String)]) -> Result<(), SecretsError> {
    if secrets.len() > MAX_REPOSITORY_SECRETS {
        return Err(SecretsError::TooManySecrets);
    }
    let mut total = 0;
    for (name, value) in secrets {
        validate_repository_secret(name, value)?;
        total += name.len() + value.len();
    }
    if total > MAX_REPOSITORY_SECRETS_BYTES {
        return Err(SecretsError::TotalTooLarge);
    }
    Ok(())
}

/// Refuses a save that would grow the repository's set past what one CI claim can carry.
/// `others` and `stored_bytes` describe the stored secrets other than the one being saved,
/// where each stored value is its plaintext plus the 16-byte AES-GCM tag.
pub(crate) fn ensure_room_for_secret(
    others: i64,
    stored_bytes: i64,
    name: &str,
    value: &str,
) -> Result<(), SecretsError> {
    const AES_GCM_TAG_BYTES: i64 = 16;
    if others >= MAX_REPOSITORY_SECRETS as i64 {
        return Err(SecretsError::TooManySecrets);
    }
    let held = stored_bytes - others * AES_GCM_TAG_BYTES;
    if held + (name.len() + value.len()) as i64 > MAX_REPOSITORY_SECRETS_BYTES as i64 {
        return Err(SecretsError::TotalTooLarge);
    }
    Ok(())
}

pub async fn create_or_update_secret(
    pool: &DbPool,
    encryptor: &SecretEncryptor,
    repo_id: i64,
    name: &str,
    value: &str,
    created_by_id: Option<i64>,
) -> Result<(), SecretsError> {
    validate_repository_secret(name, value)?;
    match pool {
        DbPool::Postgres(native) => {
            crate::backend::db::postgres::secrets::create_or_update_secret(
                native,
                encryptor,
                repo_id,
                name,
                value,
                created_by_id,
            )
            .await
        }
        DbPool::Sqlite(native) => {
            crate::backend::db::sqlite::secrets::create_or_update_secret(
                native,
                encryptor,
                repo_id,
                name,
                value,
                created_by_id,
            )
            .await
        }
    }
}

native_operation!(crate::backend::db::postgres::secrets::list_secrets,
    crate::backend::db::sqlite::secrets::list_secrets,
    fn list_secrets(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
) -> Result<Vec<RepoSecretMeta>, SecretsError>);

native_operation!(crate::backend::db::postgres::secrets::delete_secret,
    crate::backend::db::sqlite::secrets::delete_secret,
    fn delete_secret(pool: &DbPool, actor_id: i64, repo_id: i64, name: &str) -> Result<bool, SecretsError>);

// A claim with no encryption key must not silently omit configured secrets.
native_operation!(crate::backend::db::postgres::secrets::repository_has_secrets,
    crate::backend::db::sqlite::secrets::repository_has_secrets,
    fn repository_has_secrets(pool: &DbPool, repo_id: i64) -> Result<bool, SecretsError>);

// Control-plane recovery projection over an already owned DB/encryptor capability.
// Not execution delivery: no pretend job/runner arguments grant authorization.
native_operation!(crate::backend::db::postgres::secrets::decrypt_repository_secrets,
    crate::backend::db::sqlite::secrets::decrypt_repository_secrets,
    fn decrypt_repository_secrets(
    pool: &DbPool,
    encryptor: &SecretEncryptor,
    repo_id: i64,
) -> Result<Vec<(String, String)>, SecretsError>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_secret_validation_has_fixed_typed_boundaries() {
        for name in ["A", "_", "a0_Z", &"A".repeat(MAX_SECRET_NAME_BYTES)] {
            validate_repository_secret(name, "").unwrap();
        }
        for name in [
            "",
            "0TOKEN",
            "BAD-NAME",
            "BAD.NAME",
            "BAD NAME",
            "BAD/NAME",
            "BAD\nNAME",
            "TÖKEN",
            &"A".repeat(MAX_SECRET_NAME_BYTES + 1),
        ] {
            let error = validate_repository_secret(name, "value").unwrap_err();
            assert!(matches!(error, SecretsError::InvalidName));
            assert_eq!(error.to_string(), "Repository secret name is invalid");
            if !name.is_empty() {
                assert!(!error.to_string().contains(name));
            }
        }
        for name in ["GITHUB_TOKEN", "github_token", "GitHub_OIDC"] {
            let error = validate_repository_secret(name, "value").unwrap_err();
            assert!(matches!(error, SecretsError::ReservedName));
            assert_eq!(
                error.to_string(),
                "Repository secret name uses a reserved prefix"
            );
            assert!(!error.to_string().contains(name));
        }

        validate_repository_secret("BOUNDARY", &"x".repeat(MAX_SECRET_VALUE_BYTES)).unwrap();
        let marker = "private-value-that-must-not-escape";
        for (value, expected, message) in [
            (
                format!("{marker}\0tail"),
                "nul",
                "Repository secret value contains NUL",
            ),
            (
                "x".repeat(MAX_SECRET_VALUE_BYTES + 1),
                "large",
                "Repository secret value exceeds 48 KiB",
            ),
        ] {
            let error = validate_repository_secret("BOUNDARY", &value).unwrap_err();
            match expected {
                "nul" => assert!(matches!(error, SecretsError::ValueContainsNul)),
                "large" => assert!(matches!(error, SecretsError::ValueTooLarge)),
                _ => unreachable!(),
            }
            assert_eq!(error.to_string(), message);
            assert!(!error.to_string().contains(marker));
        }
    }

    #[test]
    fn repository_secret_sets_have_count_and_total_bounds() {
        let set = |count: usize, value_len: usize| -> Vec<(String, String)> {
            (0..count)
                .map(|i| (format!("SECRET_{i}"), "v".repeat(value_len)))
                .collect()
        };
        validate_repository_secrets(&set(MAX_REPOSITORY_SECRETS, 0)).unwrap();
        assert!(matches!(
            validate_repository_secrets(&set(MAX_REPOSITORY_SECRETS + 1, 0)),
            Err(SecretsError::TooManySecrets)
        ));
        // Twenty-one values of the largest size fit in 1 MiB; twenty-two do not.
        validate_repository_secrets(&set(21, MAX_SECRET_VALUE_BYTES)).unwrap();
        assert!(matches!(
            validate_repository_secrets(&set(22, MAX_SECRET_VALUE_BYTES)),
            Err(SecretsError::TotalTooLarge)
        ));
        let mut invalid = set(1, 0);
        invalid.push(("BAD-NAME".into(), "v".into()));
        assert!(matches!(
            validate_repository_secrets(&invalid),
            Err(SecretsError::InvalidName)
        ));
    }

    #[test]
    fn saving_a_secret_checks_the_stored_set() {
        const TAG: i64 = 16;
        let stored = |count: i64, value: i64| count * ("SECRET_0".len() as i64 + value + TAG);
        let value = "v".repeat(MAX_SECRET_VALUE_BYTES);
        ensure_room_for_secret(
            20,
            stored(20, MAX_SECRET_VALUE_BYTES as i64),
            "SECRET_X",
            &value,
        )
        .unwrap();
        assert!(matches!(
            ensure_room_for_secret(
                21,
                stored(21, MAX_SECRET_VALUE_BYTES as i64),
                "SECRET_X",
                &value
            ),
            Err(SecretsError::TotalTooLarge)
        ));
        assert!(matches!(
            ensure_room_for_secret(MAX_REPOSITORY_SECRETS as i64, 0, "SECRET_X", "v"),
            Err(SecretsError::TooManySecrets)
        ));
    }

    #[test]
    fn authority_context_preserves_typed_denial_and_sql_source() {
        let error = anyhow::Error::new(SecretsError::Denied).context("private SQL path");
        assert!(matches!(
            SecretsError::authority(error),
            SecretsError::Denied
        ));
        let error =
            anyhow::Error::new(crate::backend::db::discussions::DiscussionWriteRefusal::Denied)
                .context("private authority context");
        assert!(matches!(
            SecretsError::authority(error),
            SecretsError::Denied
        ));
        let error = anyhow::Error::new(sqlx::Error::Protocol("private driver payload".into()))
            .context("private SQL path");
        let error = SecretsError::authority(error);
        assert_eq!(error.to_string(), "Secret database unavailable");
        assert!(matches!(
            error,
            SecretsError::Database(sqlx::Error::Protocol(_))
        ));
        let error = SecretsError::authority(anyhow::anyhow!("private helper failure"));
        assert_eq!(error.to_string(), "Secret authority unavailable");
        assert!(std::error::Error::source(&error).is_some());
        let error = SecretsError::Encryption(SecretError::MissingMasterKey);
        assert_eq!(error.to_string(), "Secret encryption unavailable");
    }
}

//! Shared named operation surface; native transactions stay inside db.
use crate::backend::crypto::{SecretEncryptor, SecretError};
use crate::backend::db::models::RepoSecretMeta;
use crate::backend::db::DbPool;

// One bounded sample per encryption domain, from one database snapshot.
pub(crate) const ENCRYPTION_SAMPLES_SQL: &str = "
SELECT * FROM (SELECT encrypted_secret,secret_nonce FROM webhooks WHERE encrypted_secret IS NOT NULL LIMIT 1) a
UNION ALL SELECT * FROM (SELECT encrypted_password,password_nonce FROM webhooks WHERE encrypted_password IS NOT NULL LIMIT 1) b
UNION ALL SELECT * FROM (SELECT encrypted_secret,secret_nonce FROM github_app_hooks WHERE encrypted_secret IS NOT NULL LIMIT 1) c
UNION ALL SELECT * FROM (SELECT encrypted_password,password_nonce FROM email_settings WHERE encrypted_password IS NOT NULL LIMIT 1) d
UNION ALL SELECT * FROM (SELECT totp_secret_encrypted,totp_nonce FROM two_factor_auth LIMIT 1) e
UNION ALL SELECT * FROM (SELECT encrypted_value,nonce FROM repo_secrets LIMIT 1) f
UNION ALL SELECT * FROM (SELECT encrypted_key,nonce FROM repo_actions_keys LIMIT 1) g
UNION ALL SELECT * FROM (SELECT encrypted_value,nonce FROM repo_dependabot_secrets LIMIT 1) h
UNION ALL SELECT * FROM (SELECT encrypted_key,nonce FROM repo_dependabot_keys LIMIT 1) i
UNION ALL SELECT * FROM (SELECT encrypted_value,nonce FROM org_dependabot_secrets LIMIT 1) j
UNION ALL SELECT * FROM (SELECT encrypted_key,nonce FROM org_dependabot_keys LIMIT 1) k
UNION ALL SELECT * FROM (SELECT encrypted_value,nonce FROM org_actions_values WHERE kind='secret' LIMIT 1) l
UNION ALL SELECT * FROM (SELECT encrypted_key,nonce FROM org_actions_keys LIMIT 1) m
UNION ALL SELECT * FROM (SELECT encrypted_value,nonce FROM repo_environment_values WHERE kind='secret' LIMIT 1) n
UNION ALL SELECT * FROM (SELECT encrypted_secret,nonce FROM secret_scanning_alerts LIMIT 1) o
UNION ALL SELECT * FROM (SELECT encrypted_value,nonce FROM org_private_registries WHERE encrypted_value IS NOT NULL LIMIT 1) p";

native_operation!(crate::backend::db::postgres::secrets::encryption_samples,
    crate::backend::db::sqlite::secrets::encryption_samples,
    fn encryption_samples(pool: &DbPool) -> anyhow::Result<Vec<(Vec<u8>, Vec<u8>)>>);

#[derive(Clone, Copy)]
pub enum SecretKind {
    Actions,
    Dependabot,
}

impl SecretKind {
    pub(crate) fn secrets_table(self) -> &'static str {
        match self {
            Self::Actions => "repo_secrets",
            Self::Dependabot => "repo_dependabot_secrets",
        }
    }

    pub(crate) fn keys_table(self) -> &'static str {
        match self {
            Self::Actions => "repo_actions_keys",
            Self::Dependabot => "repo_dependabot_keys",
        }
    }
}

pub use codeyard_actions::values::{MAX_NAME_BYTES as MAX_SECRET_NAME_BYTES, MAX_VALUE_BYTES as MAX_SECRET_VALUE_BYTES};
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
    #[error("Secret visibility must be selected")]
    NotSelected,
    #[error("Invalid selected repositories")]
    InvalidRepositorySelection,
    #[error("Secret value is required")]
    MissingValue,
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
        if error.is::<super::users::accounts::AccountRestricted>() { return Self::Denied; }
        // Anyhow downcasts also recognize contextual errors; retain typed denial
        // and driver sources without displaying SQL, keys or helper context.
        let error = match error.downcast::<Self>() {
            Ok(error) => return error,
            Err(error) => error,
        };
        if error.is::<crate::backend::db::users::accounts::AccountRestricted>() {
            return Self::Denied;
        }
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
    use codeyard_actions::values::InvalidValue;
    codeyard_actions::values::validate(name, value).map_err(|error| match error {
        InvalidValue::Name => SecretsError::InvalidName,
        InvalidValue::ReservedName => SecretsError::ReservedName,
        InvalidValue::Nul => SecretsError::ValueContainsNul,
        InvalidValue::TooLarge => SecretsError::ValueTooLarge,
    })
}

/// Checks the whole set that one CI claim carries: each secret, then the count and the
/// total size.
pub fn validate_repository_secrets(secrets: &[(String, String)]) -> Result<(), SecretsError> {
    validate_secret_set(secrets, MAX_REPOSITORY_SECRETS)
}

/// A workflow may inherit 100 organization secrets as well as its 100 repository secrets.
/// The RPC byte bound is unchanged.
pub fn validate_workflow_secrets(secrets: &[(String, String)]) -> Result<(), SecretsError> {
    validate_secret_set(secrets, MAX_REPOSITORY_SECRETS * 2)
}

pub(crate) fn ensure_workflow_budget(count: i64, bytes: i64) -> Result<(), SecretsError> {
    if count > (MAX_REPOSITORY_SECRETS * 2) as i64 { return Err(SecretsError::TooManySecrets); }
    if bytes > MAX_REPOSITORY_SECRETS_BYTES as i64 { return Err(SecretsError::TotalTooLarge); }
    Ok(())
}

fn validate_secret_set(secrets: &[(String, String)], limit: usize) -> Result<(), SecretsError> {
    if secrets.len() > limit {
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
    credential: &super::users::accounts::AccountCredential<'_>,
) -> Result<bool, SecretsError> {
    create_or_update_secret_for(
        pool,
        encryptor,
        (repo_id, SecretKind::Actions),
        name,
        value,
        created_by_id,
        credential,
    )
    .await
}

pub async fn create_or_update_secret_for(
    pool: &DbPool,
    encryptor: &SecretEncryptor,
    repository: (i64, SecretKind),
    name: &str,
    value: &str,
    created_by_id: Option<i64>,
    credential: &super::users::accounts::AccountCredential<'_>,
) -> Result<bool, SecretsError> {
    // Secret names are case-insensitive, as on GitHub, so every writer stores them uppercase.
    let name = &name.to_ascii_uppercase();
    validate_repository_secret(name, value)?;
    match pool {
        DbPool::Postgres(native) => {
            crate::backend::db::postgres::secrets::create_or_update_secret(
                native,
                encryptor,
                repository,
                name,
                value,
                created_by_id,
                credential,
            )
            .await
        }
        DbPool::Sqlite(native) => {
            crate::backend::db::sqlite::secrets::create_or_update_secret(
                native,
                encryptor,
                repository,
                name,
                value,
                created_by_id,
                credential,
            )
            .await
        }
    }
}

pub async fn list_secrets(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
) -> Result<Vec<RepoSecretMeta>, SecretsError> {
    list_secrets_for(pool, SecretKind::Actions, actor_id, repo_id).await
}

pub async fn delete_secret(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    name: &str,
    credential: &super::users::accounts::AccountCredential<'_>,
) -> Result<bool, SecretsError> {
    // An upgrade keeps an old lowercase name whose uppercase form was taken, so the exact name
    // is deleted first.
    let upper = name.to_ascii_uppercase();
    Ok(
        delete_secret_for(pool, SecretKind::Actions, actor_id, repo_id, name, credential).await?
            || (upper != name
                && delete_secret_for(pool, SecretKind::Actions, actor_id, repo_id, &upper, credential).await?),
    )
}

native_operation!(crate::backend::db::postgres::secrets::repository_secret_key,
    crate::backend::db::sqlite::secrets::repository_secret_key,
    fn repository_secret_key_for(
    pool: &DbPool,
    kind: SecretKind,
    encryptor: &SecretEncryptor,
    actor_id: i64,
    repo_id: i64,
    credential: &super::users::accounts::AccountCredential<'_>,
) -> Result<(String, crypto_box::SecretKey), SecretsError>);

native_operation!(crate::backend::db::postgres::secrets::list_secrets,
    crate::backend::db::sqlite::secrets::list_secrets,
    fn list_secrets_for(
    pool: &DbPool,
    kind: SecretKind,
    actor_id: i64,
    repo_id: i64,
) -> Result<Vec<RepoSecretMeta>, SecretsError>);

native_operation!(crate::backend::db::postgres::secrets::delete_secret,
    crate::backend::db::sqlite::secrets::delete_secret,
    fn delete_secret_for(pool: &DbPool, kind: SecretKind, actor_id: i64, repo_id: i64, name: &str, credential: &super::users::accounts::AccountCredential<'_>) -> Result<bool, SecretsError>);

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
        let error = SecretsError::Encryption(SecretError::EncryptionFailed);
        assert_eq!(error.to_string(), "Secret encryption unavailable");
    }
}

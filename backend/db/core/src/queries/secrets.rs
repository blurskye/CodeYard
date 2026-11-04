use codeyard_actions::values::MAX_SCOPE_VALUES as MAX_REPOSITORY_SECRETS;
use codeyard_actions::values::MAX_SECRET_BYTES as MAX_REPOSITORY_SECRETS_BYTES;
use codeyard_support::secrets::SecretError;
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
    pub fn authority(error: anyhow::Error) -> Self {
        if error.is::<codeyard_support::credentials::AccountRestricted>() {
            return Self::Denied;
        }
        // Anyhow downcasts also recognize contextual errors; retain typed denial
        // and driver sources without displaying SQL, keys or helper context.
        let error = match error.downcast::<Self>() {
            Ok(error) => return error,
            Err(error) => error,
        };
        if error.is::<codeyard_support::credentials::AccountRestricted>() {
            return Self::Denied;
        }
        if matches!(
            error.downcast_ref::<crate::queries::discussions::DiscussionWriteRefusal>(),
            Some(crate::queries::discussions::DiscussionWriteRefusal::Denied)
        ) {
            return Self::Denied;
        }
        match error.downcast::<sqlx::Error>() {
            Ok(error) => Self::Database(error),
            Err(error) => Self::Authority(error),
        }
    }
}

pub(crate) fn ensure_workflow_budget(count: i64, bytes: i64) -> Result<(), SecretsError> {
    if count > (MAX_REPOSITORY_SECRETS * 2) as i64 {
        return Err(SecretsError::TooManySecrets);
    }
    if bytes > MAX_REPOSITORY_SECRETS_BYTES as i64 {
        return Err(SecretsError::TotalTooLarge);
    }
    Ok(())
}

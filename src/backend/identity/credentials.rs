use super::passwords::MAX_PASSWORD_BYTES;

/// Borrowed credentials deliberately have no Debug/Serialize implementation.
pub struct PasswordAccount<'a> {
    pub username: &'a str,
    pub email: Option<&'a str>,
    pub password: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AccountInputError {
    UsernameBounds,
    UsernameCharacters,
    UsernameReserved,
    PasswordBounds,
    EmailBounds,
    EmailFormat,
}

pub(crate) fn validate_namespace(username: &str, min_bytes: usize) -> Result<(), AccountInputError> {
    // PostgreSQL VARCHAR(255) is a character bound, not a UTF-8 byte bound.
    // Non-ASCII usernames pass this check; the byte limit keeps request work bounded.
    if !(min_bytes..=1024).contains(&username.len()) || username.chars().count() > 255 {
        return Err(AccountInputError::UsernameBounds);
    }
    if !username
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(AccountInputError::UsernameCharacters);
    }
    Ok(())
}

pub(crate) fn validate_account(input: &PasswordAccount<'_>) -> Result<(), AccountInputError> {
    validate_namespace(input.username, 3)?;
    validate_new_password(input.password)?;
    // A blank optional address means none; the account is saved without one.
    if let Some(email) = input.email.filter(|email| !email.trim().is_empty()) {
        validate_email(email)?;
    }
    Ok(())
}

pub(crate) fn validate_email(email: &str) -> Result<(), AccountInputError> {
    if email.len() > 255 || email.contains('\0') {
        return Err(AccountInputError::EmailBounds);
    }
    // Mail is sent to this address, so it must parse as one.
    if email.parse::<lettre::Address>().is_err() {
        return Err(AccountInputError::EmailFormat);
    }
    Ok(())
}

/// Reserved names are checked only when a new account is created. An existing account
/// keeps its name, so setup can still finish for an administrator named `admin`.
pub(crate) fn validate_new_account_name(username: &str) -> Result<(), AccountInputError> {
    if crate::backend::db::users::is_reserved_name(username) {
        return Err(AccountInputError::UsernameReserved);
    }
    Ok(())
}

pub(crate) fn validate_new_password(password: &str) -> Result<(), AccountInputError> {
    if !(8..=MAX_PASSWORD_BYTES).contains(&password.len()) {
        return Err(AccountInputError::PasswordBounds);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_validation_rejects_nul() {
        assert_eq!(
            validate_email("a\0@example.test"),
            Err(AccountInputError::EmailBounds)
        );
        assert_eq!(validate_email("alice@example.test"), Ok(()));
    }

    #[test]
    fn account_validation_preserves_unicode_and_byte_password_bounds() {
        fn account(username: &str) -> PasswordAccount<'_> {
            PasswordAccount {
                username,
                email: None,
                password: "password",
            }
        }
        assert_eq!(validate_account(&account("éé")), Ok(()));
        assert_eq!(
            validate_account(&account(&"é".repeat(256))),
            Err(AccountInputError::UsernameBounds)
        );
        assert_eq!(
            validate_account(&account("a/b")),
            Err(AccountInputError::UsernameCharacters)
        );
        // The account rules accept reserved names; only creating an account checks them.
        assert_eq!(validate_account(&account("Settings")), Ok(()));
        assert_eq!(
            validate_new_account_name("Settings"),
            Err(AccountInputError::UsernameReserved)
        );
        assert_eq!(validate_new_account_name("root_admin"), Ok(()));
        assert_eq!(validate_new_password("éééé"), Ok(()));
        assert_eq!(
            validate_new_password(&"x".repeat(1025)),
            Err(AccountInputError::PasswordBounds)
        );
    }
}

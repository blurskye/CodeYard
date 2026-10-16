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
}

pub(crate) fn validate_username(username: &str) -> Result<(), AccountInputError> {
    // PostgreSQL VARCHAR(255) is a character bound, not a UTF-8 byte bound.
    // Preserve existing non-ASCII identities while bounding request work.
    if !(3..=1024).contains(&username.len()) || username.chars().count() > 255 {
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
    validate_username(input.username)?;
    validate_new_password(input.password)?;
    if input.email.is_some_and(|email| email.len() > 255) {
        return Err(AccountInputError::EmailBounds);
    }
    Ok(())
}

/// Reserved names guard only the creation of a new account. An existing account keeps
/// its name, so setup recovery for an administrator called `admin` can still finish.
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
        // Existing names pass the account rules; only creating one is reserved-checked.
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

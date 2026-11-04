#[derive(Debug, PartialEq, Eq)]
pub enum AccountInputError {
    UsernameBounds,
    UsernameCharacters,
    UsernameReserved,
    PasswordBounds,
    EmailBounds,
    EmailFormat,
}

pub fn validate_namespace(username: &str, min_bytes: usize) -> Result<(), AccountInputError> {
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

/// The one rule for a bare email address that is stored or mailed: at most 255 bytes, no
/// whitespace or control characters, and an address mail can be sent to.
pub fn validate_email(email: &str) -> Result<(), AccountInputError> {
    if email.len() > 255 || email.contains('\0') {
        return Err(AccountInputError::EmailBounds);
    }
    if email
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
        || email.parse::<lettre::Address>().is_err()
    {
        return Err(AccountInputError::EmailFormat);
    }
    Ok(())
}

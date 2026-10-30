//! Named Actions value contracts shared by storage, claim producers and native consumers.
pub const MAX_NAME_BYTES: usize = 255;
pub const MAX_VALUE_BYTES: usize = 48 * 1024;
/// Per environment map and per repository/organization secret set; variables at those
/// broader scopes have their own higher count limits.
pub const MAX_SCOPE_VALUES: usize = 100;
pub const MAX_SECRET_BYTES: usize = 1024 * 1024;
pub const MAX_VARIABLE_BYTES: usize = 256 * 1024;
pub const MAX_ENVIRONMENT_FILE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidValue {
    Name,
    ReservedName,
    Nul,
    TooLarge,
}

pub fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    !name.is_empty() && name.len() <= MAX_NAME_BYTES
        && bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub fn reserved_name(name: &str) -> bool {
    name.get(.."GITHUB_".len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case("GITHUB_"))
}

pub fn validate(name: &str, value: &str) -> Result<(), InvalidValue> {
    if !valid_name(name) {
        return Err(InvalidValue::Name);
    }
    if reserved_name(name) {
        return Err(InvalidValue::ReservedName);
    }
    if value.contains('\0') {
        return Err(InvalidValue::Nul);
    }
    if value.len() > MAX_VALUE_BYTES {
        return Err(InvalidValue::TooLarge);
    }
    Ok(())
}

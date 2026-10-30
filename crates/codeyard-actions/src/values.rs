//! Per-entry constraints shared by stored Actions values and runner delivery files.
pub const MAX_NAME_BYTES: usize = 255;
pub const MAX_VALUE_BYTES: usize = 48 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidValue {
    Name,
    ReservedName,
    Nul,
    TooLarge,
}

pub fn validate(name: &str, value: &str) -> Result<(), InvalidValue> {
    let mut bytes = name.bytes();
    let first = bytes.next().ok_or(InvalidValue::Name)?;
    if name.len() > MAX_NAME_BYTES
        || !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(InvalidValue::Name);
    }
    if name.to_ascii_uppercase().starts_with("GITHUB_") {
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

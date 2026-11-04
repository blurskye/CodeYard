use crate::shared::user::responses::field;
use crate::shared::user::responses::Fields;
/// An optional whole number from a settings form: empty is `None`, anything else must lie in
/// `1..=max`. Repository settings read their limits with it too.
pub fn optional_number(
    fields: &Fields,
    name: &str,
    label: &str,
    max: i32,
) -> Result<Option<i32>, String> {
    let text = field(fields, name).trim();
    if text.is_empty() {
        return Ok(None);
    }
    match text.parse::<i32>() {
        Ok(value) if (1..=max).contains(&value) => Ok(Some(value)),
        _ => Err(format!("{label} must be a whole number from 1 to {max}.")),
    }
}

/// The limits' upper bounds, as the schema checks them.
pub const MAX_TIMEOUT_MINUTES: i32 = 10080;

pub const MAX_CPUS: i32 = 1024;

pub const MAX_MEMORY_GIB: i32 = 4096;

pub fn text(value: Option<i32>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

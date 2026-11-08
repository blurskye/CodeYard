use serde_json::Value;
pub fn allows(permissions: &Value, key: &str, write: bool) -> bool {
    permissions
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|level| level == "admin" || level == "write" || (!write && level == "read"))
}

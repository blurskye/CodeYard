use std::collections::HashMap;

/// The values a repeated form field was sent with, in the order they were sent.
pub(crate) fn repeated(pairs: &[(String, String)], name: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
        .collect()
}

/// A native form sends a checked checkbox as "on" and an unchecked one not at all.
pub(crate) fn checkbox(form: &HashMap<String, String>, name: &str) -> bool {
    matches!(form.get(name).map(String::as_str), Some("on" | "true"))
}

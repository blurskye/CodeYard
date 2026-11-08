pub fn valid_basename(name: &str, limit: usize) -> bool {
    !name.is_empty()
        && name.len() <= limit
        && !matches!(name, "." | "..")
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

//! Repository-name admission shared by native authority and browser feedback.

pub fn valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !matches!(name, "." | "..")
        && !name.ends_with(".git")
        && name.chars().all(|character| character.is_alphanumeric() || matches!(character, '-' | '_' | '.'))
}

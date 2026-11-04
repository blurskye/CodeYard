//! Text matching for SQLite. Its LIKE folds ASCII letters only, so a search compiles its text to a
//! case-insensitive Unicode regular expression. The `regexp` function that sqlx registers on every
//! connection runs it in SQL, as `column REGEXP pattern`, which keeps filtering, ordering, paging
//! and counting in one query.

/// The characters a regular expression gives a meaning, which a literal search must escape.
const REGEX_META: &str = r"\.+*?()|[]{}^$#&-~";

/// The pattern that matches `text` as a literal substring, ignoring case.
pub fn contains(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len() + 8);
    pattern.push_str("(?i)");
    for character in text.chars() {
        if REGEX_META.contains(character) {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern
}

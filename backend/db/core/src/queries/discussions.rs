#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum DiscussionWriteRefusal {
    #[error("discussion_authority_denied")]
    Denied,
    #[error("conversation_locked")]
    Locked,
    #[error("discussion_review_invalid")]
    InvalidReview,
    #[error("discussion_review_full")]
    ReviewFull,
    /// A review other than an approval with neither a body nor inline comments.
    #[error("discussion_review_empty")]
    EmptyReview,
    #[error("discussion_revision_changed")]
    RevisionChanged,
}

/// The `ILIKE` pattern, for PostgreSQL, for a title or body that contains `text`. The search text
/// matches itself, so its `\`, `%` and `_` are escaped and the query uses `ESCAPE '\'`. SQLite
/// builds its own pattern in `sqlite::search`.
pub fn contains_pattern(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len() + 2);
    pattern.push('%');
    for character in text.chars() {
        if matches!(character, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

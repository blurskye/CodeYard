//! Credential-free HTTP(S) addresses for public website fields and links.

/// Field owners decide whether to trim input, allow a blank value or bound its length.
pub fn parse(value: &str) -> Option<url::Url> {
    if value
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

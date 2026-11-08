//! Link admission shared by Markdown rendering and stored link templates.

/// Whether a link or image may point at `url`: http(s), relative paths and, for links only,
/// mailto. The scheme is what precedes a colon that comes before the first `/`, `?` or `#`.
pub fn safe_url(url: &str, image: bool) -> bool {
    if url.is_empty()
        || url.starts_with("//")
        || url
            .chars()
            .any(|c| (c.is_whitespace() && c != ' ') || c.is_control() || c == '\\')
    {
        return false;
    }
    let boundary = url.find(['/', '?', '#']).unwrap_or(url.len());
    let Some(colon) = url[..boundary].find(':') else {
        return true;
    };
    let scheme = &url[..colon];
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        return url[colon + 1..].starts_with("//");
    }
    !image && scheme.eq_ignore_ascii_case("mailto")
}

/// Admission uses the same raw-spelling policy as Markdown rendering.
pub fn valid_autolink_template(template: &str) -> bool {
    template.len() <= 4096 && safe_url(&template.replace("<num>", "1"), false)
}

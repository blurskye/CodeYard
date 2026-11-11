//! Percent-encoding of URL path segments and the product paths built from them.

/// Percent-encode one path segment or query value, never an entire URL.
pub fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(encoded, "%{byte:02X}").expect("writing to String");
        }
    }
    encoded
}

/// `%XX` escapes decoded once; bytes that are not valid escapes stay as they are.
pub fn decode_component(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escaped = (bytes[at] == b'%')
            .then(|| bytes.get(at + 1..at + 3))
            .flatten()
            .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `/gists` is now a product page; keep preexisting accounts and repositories addressable.
pub fn profile_path(username: &str) -> String {
    let name = encode_component(username);
    if username.eq_ignore_ascii_case("gists") {
        format!("/gists/@users/{name}")
    } else {
        format!("/{name}")
    }
}

pub fn repository_path(owner: &str, name: &str) -> String {
    let prefix = if owner.eq_ignore_ascii_case("gists") {
        "/gists/@repos"
    } else {
        ""
    };
    format!(
        "{prefix}/{}/{}",
        encode_component(owner),
        encode_component(name)
    )
}

pub fn commit_comment_path(owner: &str, name: &str, sha: &str, id: i64) -> String {
    format!(
        "/{}/{}/commit/{sha}?comment={id}#commitcomment-{id}",
        encode_component(owner),
        encode_component(name)
    )
}

/// Display one workflow filename, removing exactly one final YAML extension.
pub fn workflow_filename(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".yml")
        .or_else(|| name.strip_suffix(".yaml"))
        .unwrap_or(name)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encodes_segments_and_utf8_without_query_ambiguity() {
        assert_eq!(encode_component("a/b +é?"), "a%2Fb%20%2B%C3%A9%3F");
    }

    #[test]
    fn commit_comment_paths_preserve_the_query_and_anchor() {
        assert_eq!(
            commit_comment_path("owner", "repo", "abc123", 42),
            "/owner/repo/commit/abc123?comment=42#commitcomment-42"
        );
        assert_eq!(
            commit_comment_path("a/b", "repo ?é", "abc123", 42),
            "/a%2Fb/repo%20%3F%C3%A9/commit/abc123?comment=42#commitcomment-42"
        );
    }
}

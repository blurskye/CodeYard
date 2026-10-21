//! Canonical branch and tag reference names used at every publication boundary.

/// Returns whether `reference` is an exact, non-normalized branch or tag ref.
/// Git's normalization can collapse empty path components, so callers reject
/// malformed names rather than retargeting them to another ref.
pub(crate) fn is_canonical_ref(reference: &str) -> bool {
    (reference.starts_with("refs/heads/") || reference.starts_with("refs/tags/"))
        && reference.len() <= 1024
        && !reference.bytes().any(|byte| {
            byte <= b' '
                || byte == 127
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
        && !reference.contains("..")
        && !reference.contains("@{")
        && reference.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && !part.ends_with('.')
                && !part.ends_with(".lock")
        })
        && git2::Reference::is_valid_name(reference)
}

/// Whether `text` is a full 40-digit commit ID. Either case passes; a caller that stores the
/// ID lowercases it.
pub(crate) fn is_full_commit_id(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn branch_ref(name: &str) -> Option<String> {
    let reference = format!("refs/heads/{name}");
    is_canonical_ref(&reference).then_some(reference)
}

pub(crate) fn tag_ref(name: &str) -> Option<String> {
    let reference = format!("refs/tags/{name}");
    is_canonical_ref(&reference).then_some(reference)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_commit_id_is_exactly_forty_hex_digits() {
        assert!(is_full_commit_id(&"a".repeat(40)));
        assert!(is_full_commit_id(&"A1".repeat(20)));
        assert!(!is_full_commit_id(&"a".repeat(39)));
        assert!(!is_full_commit_id(&"a".repeat(41)));
        assert!(!is_full_commit_id(&format!("{}g", "a".repeat(39))));
    }

    #[test]
    fn refs_are_accepted_only_in_canonical_branch_and_tag_form() {
        for reference in [
            "refs/heads/main",
            "refs/heads/release/stable",
            "refs/heads/équipe/功能",
            "refs/tags/v1",
            "refs/tags/版本-β",
            "refs/tags/release/stable",
        ] {
            assert!(is_canonical_ref(reference), "{reference}");
        }
        for reference in [
            "refs/heads/release//stable",
            "refs/heads//release/stable",
            "refs/heads/release/stable.",
            "refs/heads/release/.stable",
            "refs/heads/release/stable.lock",
            "refs/tags/release//stable",
            "refs/remotes/origin/main",
        ] {
            assert!(!is_canonical_ref(reference), "{reference}");
        }
    }
}

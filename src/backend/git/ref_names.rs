//! Canonical public reference names used at publication boundaries.

/// libgit2's 1024-byte reference buffer also reserves a terminating NUL.
pub(crate) const MAX_REF_BYTES: usize = 1023;

pub(crate) const PUBLIC_REF_NAMESPACES: [&str; 3] = ["refs/heads/", "refs/tags/", "refs/notes/"];

/// Namespace selection only; callers admitting a new name must also validate it.
pub(crate) fn is_public_ref(reference: &str) -> bool {
    PUBLIC_REF_NAMESPACES
        .iter()
        .any(|prefix| reference.starts_with(*prefix))
}

/// Returns whether `reference` is an exact, non-normalized branch, tag or notes ref.
/// Git's normalization can collapse empty path components, so callers reject
/// malformed names rather than retargeting them to another ref.
pub(crate) fn is_canonical_ref(reference: &str) -> bool {
    is_public_ref(reference)
        && reference.len() <= MAX_REF_BYTES
        && !reference.bytes().any(|byte| {
            byte <= b' '
                || byte == 127
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
        && !reference.contains("..")
        && !reference.contains("@{")
        && reference
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
        && git2::Reference::is_valid_name(reference)
}

pub(crate) use codeyard_support::git_ids::is_full_commit_id;

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
        assert!(is_full_commit_id("a".repeat(40)));
        assert!(is_full_commit_id("A1".repeat(20)));
        assert!(!is_full_commit_id("a".repeat(39)));
        assert!(!is_full_commit_id("a".repeat(41)));
        assert!(!is_full_commit_id(format!("{}g", "a".repeat(39))));
    }

    #[test]
    fn refs_are_accepted_only_in_canonical_branch_and_tag_form() {
        for reference in [
            "refs/heads/main",
            "refs/heads/release/stable",
            "refs/heads/release./stable",
            "refs/heads/équipe/功能",
            "refs/tags/v1",
            "refs/tags/版本-β",
            "refs/tags/release/stable",
            "refs/notes/commits",
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

//! Longest-known-ref resolver preserved from the former pure repo_ref_path helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRefPath {
    pub reference: String,
    pub path: String,
}

pub fn resolve_repo_ref_path(
    parts: &[String],
    refs: &[String],
    require_path: bool,
) -> Option<ResolvedRefPath> {
    if parts.is_empty() || (require_path && parts.len() < 2) {
        return None;
    }
    for index in (1..=parts.len()).rev() {
        let reference = parts[..index].join("/");
        let path = parts[index..].join("/");
        if refs.contains(&reference) && (!require_path || !path.is_empty()) {
            return Some(ResolvedRefPath { reference, path });
        }
    }
    Some(ResolvedRefPath {
        reference: parts[0].clone(),
        path: parts[1..].join("/"),
    })
}

pub fn split_path(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Encode each filename/ref segment, keeping only actual path separators.
pub fn encode_path(path: &str) -> String {
    path.split('/')
        .map(crate::webui::encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether a file's path names Markdown. The blob view shows such a file as a document, and the
/// file editor previews it that way too, so both use this rule.
pub fn is_markdown_path(path: &str) -> bool {
    let path = path.to_lowercase();
    [".md", ".markdown", ".mdx"]
        .iter()
        .any(|extension| path.ends_with(*extension))
}

pub fn file_url(base: &str, kind: &str, reference: &str, path: &str) -> String {
    let tail = if path.is_empty() {
        String::new()
    } else {
        format!("/{}", encode_path(path))
    };
    format!("{base}/{kind}/{}{tail}", encode_path(reference))
}

/// The comparison of two branches of one repository. Git never allows `..` in a branch name, so
/// the first `...` in the address is the separator.
pub fn compare_url(repo_url: &str, base: &str, head: &str) -> String {
    format!(
        "{repo_url}/compare/{}...{}",
        encode_path(base),
        encode_path(head)
    )
}

// Real bases are a few hundred bytes; a longer one would be copied into every link.
const MAX_LINK_BASE_BYTES: usize = 4096;

/// Where relative links in a Markdown file under `directory` of `reference` point. A base that
/// is too long is replaced by the default, and then links stay as written.
pub fn link_base(
    base: &str,
    reference: &str,
    directory: &str,
) -> crate::webui::components::LinkBase {
    let blob = file_url(base, "blob", reference, "");
    let directory = encode_path(directory.trim_matches('/'));
    // A resolved link copies the directory and one of the blob, tree or raw URLs. The blob URL
    // is the longest of them, so a longer base is left out before the others are built.
    if blob.len() + directory.len() > MAX_LINK_BASE_BYTES {
        return crate::webui::components::LinkBase::default();
    }
    crate::webui::components::LinkBase {
        blob,
        tree: file_url(base, "tree", reference, ""),
        raw: file_url(base, "raw", reference, ""),
        directory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn longest_branch_and_tag_are_preserved() {
        let refs = vec!["feature".into(), "feature/x".into(), "v1/release".into()];
        let result =
            resolve_repo_ref_path(&split_path("feature/x/docs/a b.md"), &refs, true).unwrap();
        assert_eq!(result.reference, "feature/x");
        assert_eq!(result.path, "docs/a b.md");
        assert_eq!(
            resolve_repo_ref_path(&split_path("v1/release"), &refs, false)
                .unwrap()
                .reference,
            "v1/release"
        );
    }
    #[test]
    fn missing_path_and_unknown_ref() {
        assert_eq!(resolve_repo_ref_path(&[], &[], false), None);
        assert_eq!(resolve_repo_ref_path(&split_path("main"), &[], true), None);
        assert_eq!(
            resolve_repo_ref_path(&split_path("abc/a"), &[], true)
                .unwrap()
                .reference,
            "abc"
        );
    }
    #[test]
    fn urls_encode_filename_delimiters_without_losing_ref_slashes() {
        assert_eq!(
            file_url("/u/r", "blob", "feature/x", "a #?.rs"),
            "/u/r/blob/feature/x/a%20%23%3F.rs"
        );
    }
}

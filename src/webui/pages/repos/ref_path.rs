//! Splits the path of a repository address into the longest known ref and the path inside it.
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
    if parts.is_empty()
        || parts
            .iter()
            .map(|part| part.len().saturating_add(1))
            .sum::<usize>()
            > 4096
        || (require_path && parts.len() < 2)
    {
        return None;
    }
    // A full object ID is an immutable address, never a prefix of a named ref.
    let tail = parts.join("/");
    let commit = parts[0].len() == 40 && parts[0].bytes().all(|byte| byte.is_ascii_hexdigit());
    let reference = if commit {
        parts[0].as_str()
    } else {
        refs.iter()
            .filter(|reference| {
                tail == **reference && !require_path
                    || tail
                        .strip_prefix(reference.as_str())
                        .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1)
            })
            .max_by_key(|reference| reference.len())
            .map_or(parts[0].as_str(), String::as_str)
    };
    Some(ResolvedRefPath {
        reference: reference.to_owned(),
        path: tail
            .get(reference.len()..)
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_owned(),
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

/// The query a ref switch adds to a file or folder link. When the path is missing on the chosen
/// ref, the reader lands on that ref's root instead of an error page.
pub const ROOT_IF_MISSING: &str = "root_if_missing";

pub fn file_url(base: &str, kind: &str, reference: &str, path: &str) -> String {
    let tail = if path.is_empty() {
        String::new()
    } else {
        format!("/{}", encode_path(path))
    };
    // Git forbids prefix collisions within one namespace (main and main/docs), but permits
    // them across heads and tags. Qualifying the ref keeps filenames out of the other namespace.
    let qualified;
    let reference = if !path.is_empty() {
        qualified = qualified_ref(reference);
        qualified.as_str()
    } else {
        reference
    };
    format!("{base}/{kind}/{}{tail}", encode_path(reference))
}

fn qualified_ref(reference: &str) -> String {
    if reference.starts_with("refs/")
        || (reference.len() == 40 && reference.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        reference.to_owned()
    } else {
        format!("refs/heads/{reference}")
    }
}

/// The link a ref menu uses for `path` on `reference`: the same path when the ref has it, else
/// the ref's root. The root link needs no such query.
pub fn ref_switch_url(base: &str, kind: &str, reference: &str, path: &str) -> String {
    let url = file_url(base, kind, reference, path);
    if path.is_empty() {
        url
    } else {
        format!("{url}?{ROOT_IF_MISSING}=1")
    }
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
    let reference = qualified_ref(reference);
    let reference = reference.as_str();
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
    fn full_commit_ids_precede_named_ref_prefixes() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let refs = vec![format!("{sha}/docs")];
        let resolved =
            resolve_repo_ref_path(&split_path(&format!("{sha}/docs/a.rs")), &refs, true).unwrap();
        assert_eq!(resolved.reference, sha);
        assert_eq!(resolved.path, "docs/a.rs");
    }

    #[test]
    fn oversized_tails_are_refused_before_prefix_matching() {
        assert!(resolve_repo_ref_path(
            &split_path(&format!("main/{}", "a/".repeat(50_000))),
            &["main".into()],
            false
        )
        .is_none());
    }

    #[test]
    fn named_hex_refs_stay_qualified_in_file_and_markdown_links() {
        assert_eq!(
            file_url("/u/r", "blob", "abcdef1", "docs/a.md"),
            "/u/r/blob/refs/heads/abcdef1/docs/a.md"
        );
        let base = link_base("/u/r", "refs/heads/abcdef1", "");
        assert_eq!(base.blob, "/u/r/blob/refs/heads/abcdef1");
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            file_url("/u/r", "blob", sha, "a.md"),
            format!("/u/r/blob/{sha}/a.md")
        );
    }

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
    fn generated_links_keep_branch_paths_out_of_the_tag_namespace() {
        let refs = vec![
            "main".into(),
            "main/docs".into(),
            "refs/heads/main".into(),
            "refs/tags/main/docs".into(),
        ];
        let url = file_url("/u/r", "blob", "main", "docs/a.md");
        let tail = url.strip_prefix("/u/r/blob/").unwrap();
        let resolved = resolve_repo_ref_path(&split_path(tail), &refs, true).unwrap();
        assert_eq!(resolved.reference, "refs/heads/main");
        assert_eq!(resolved.path, "docs/a.md");
        let tag = file_url("/u/r", "tree", "refs/tags/main/docs", "");
        assert!(tag.ends_with("/refs/tags/main/docs"));
    }

    #[test]
    fn urls_encode_filename_delimiters_without_losing_ref_slashes() {
        assert_eq!(
            file_url("/u/r", "blob", "feature/x", "a #?.rs"),
            "/u/r/blob/refs/heads/feature/x/a%20%23%3F.rs"
        );
    }
}

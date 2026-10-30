//! npm package-name grammar shared by hosted registries and dependency metadata.
pub(crate) fn package_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 214 { return false; }
    let parts: Vec<_> = name.strip_prefix('@').unwrap_or(name).split('/').collect();
    if parts.len() != if name.starts_with('@') { 2 } else { 1 } { return false; }
    parts.iter().all(|part| !part.is_empty() && !part.starts_with(['.', '_'])
        && part.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._~-".contains(&byte)))
}

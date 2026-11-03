//! Submitted dependency graphs and immutable SBOM reports.
use anyhow::Result;
use chrono::DateTime;
use serde_json::Value;

pub const MAX_VERSION_BYTES: usize = 256;

/// Package URL components shared by validation and inventory presentation.
pub fn package_parts(purl: &str) -> Option<(&str, &str, &str)> {
    let (ecosystem, qualified) = purl.strip_prefix("pkg:")?.split_once('/')?;
    if !ecosystem.as_bytes().first()?.is_ascii_lowercase()
        || !ecosystem
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".+-".contains(&b))
    {
        return None;
    }
    let qualified = qualified.split(['?', '#']).next()?;
    let (name, version) = match qualified.rsplit_once('@') {
        Some((_, "")) => return None,
        Some(parts) => parts,
        None => (qualified, ""),
    };
    if name
        .split('/')
        .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return None;
    }
    for (index, byte) in purl.bytes().enumerate() {
        if byte == b'%'
            && !purl
                .as_bytes()
                .get(index + 1..index + 3)?
                .iter()
                .all(u8::is_ascii_hexdigit)
        {
            return None;
        }
    }
    if codeyard_webui::decode_component(version).len() > MAX_VERSION_BYTES {
        return None;
    }
    for part in [name, version] {
        if codeyard_webui::decode_component(part)
            .chars()
            .any(char::is_control)
        {
            return None;
        }
    }
    Some((ecosystem, name, version))
}

pub fn validate(document: &str) -> Result<Value> {
    use anyhow::ensure;
    ensure!(document.len() <= 4 * 1024 * 1024, Refusal::Invalid);
    let value: Value = serde_json::from_str(document).map_err(|_| Refusal::Invalid)?;
    let text = |v: &Value, max: usize| {
        v.as_str()
            .is_some_and(|s| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control))
    };
    ensure!(
        value.is_object() && value["version"].as_i64().is_some_and(|v| v >= 0),
        Refusal::Invalid
    );
    ensure!(
        text(&value["sha"], 64)
            && value["sha"].as_str().is_some_and(
                |s| (40..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
            ),
        Refusal::Invalid
    );
    ensure!(
        text(&value["ref"], 1024)
            && value["ref"]
                .as_str()
                .is_some_and(|s| s.starts_with("refs/") && git2::Reference::is_valid_name(s)),
        Refusal::Invalid
    );
    for (part, keys) in [
        ("job", &["id", "correlator"][..]),
        ("detector", &["name", "version", "url"][..]),
    ] {
        ensure!(
            value[part].is_object()
                && keys
                    .iter()
                    .all(|key| text(&value[part][key], if *key == "url" { 2048 } else { 255 })),
            Refusal::Invalid
        );
    }
    ensure!(
        value["scanned"]
            .as_str()
            .is_some_and(|s| DateTime::parse_from_rfc3339(s).is_ok()),
        Refusal::Invalid
    );
    if let Some(manifests) = value.get("manifests") {
        let manifests = manifests.as_object().ok_or(Refusal::Invalid)?;
        ensure!(manifests.len() <= 100, Refusal::Invalid);
        let mut nodes = 0;
        for (path, manifest) in manifests {
            ensure!(
                !path.is_empty() && path.len() <= 2048 && text(&manifest["name"], 2048),
                Refusal::Invalid
            );
            if let Some(resolved) = manifest.get("resolved") {
                let resolved = resolved.as_object().ok_or(Refusal::Invalid)?;
                nodes += resolved.len();
                ensure!(nodes <= 2000, Refusal::Invalid);
                for (id, dependency) in resolved {
                    ensure!(id.len() <= 2048 && dependency.is_object(), Refusal::Invalid);
                    if let Some(purl) = dependency.get("package_url") {
                        ensure!(
                            text(purl, 4096)
                                && purl.as_str().is_some_and(|s| package_parts(s).is_some()),
                            Refusal::Invalid
                        );
                    }
                    if let Some(scope) = dependency.get("scope") {
                        ensure!(
                            matches!(scope.as_str(), Some("runtime" | "development")),
                            Refusal::Invalid
                        );
                    }
                    if let Some(relationship) = dependency.get("relationship") {
                        ensure!(
                            matches!(relationship.as_str(), Some("direct" | "indirect")),
                            Refusal::Invalid
                        );
                    }
                    if let Some(deps) = dependency.get("dependencies") {
                        let deps = deps.as_array().ok_or(Refusal::Invalid)?;
                        ensure!(
                            deps.len() <= 2000
                                && deps.iter().all(|id| id
                                    .as_str()
                                    .is_some_and(|id| resolved.contains_key(id))),
                            Refusal::Invalid
                        );
                    }
                }
            }
        }
    }
    Ok(value)
}

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Dependency graph mutation denied")]
    Denied,
    #[error("Invalid dependency graph")]
    Invalid,
}

#[cfg(test)]
mod tests {
    use super::package_parts;
    #[test]
    fn package_urls_require_identity_and_preserve_encoded_components() {
        assert_eq!(
            package_parts("pkg:npm/%40scope/name@1.0.0%2Bbuild?arch=x"),
            Some(("npm", "%40scope/name", "1.0.0%2Bbuild"))
        );
        for invalid in [
            "pkg:/",
            "pkg:npm/",
            "pkg:npm//name@1",
            "pkg:npm/name@",
            "pkg:npm/name%00@1",
            "pkg:npm/name%xx@1",
        ] {
            assert!(package_parts(invalid).is_none(), "{invalid}");
        }
    }
}

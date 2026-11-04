use crate::npm::dependency_name;
use crate::repositories::dependencies::npm::formatted;
use crate::repositories::dependencies::npm::json;
use crate::repositories::dependencies::npm::package;
use crate::repositories::dependencies::npm::registry_url;
use crate::repositories::dependencies::npm::Archive;
use crate::repositories::dependencies::npm::Lockfile;
use serde_json::json;
use sha2::Digest;

fn archive(document: Value) -> (Value, Vec<u8>) {
    archive_extra(document, None)
}
fn archive_extra(document: Value, extra: Option<(&str, tar::EntryType)>) -> (Value, Vec<u8>) {
    let bytes = serde_json::to_vec(&document).unwrap();
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_data(&mut header, "package/package.json", bytes.as_slice())
        .unwrap();
    if let Some((path, kind)) = extra {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(0o644);
        let bytes = if kind.is_file() {
            b"unsafe".as_slice()
        } else {
            b"".as_slice()
        };
        header.set_size(bytes.len() as u64);
        if kind.is_symlink() || kind.is_hard_link() {
            header.set_link_name("package/package.json").unwrap();
        }
        header.set_cksum();
        builder.append_data(&mut header, path, bytes).unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tar).unwrap();
    let bytes = encoder.finish().unwrap();
    let integrity = format!("sha512-{}", STANDARD.encode(Sha512::digest(&bytes)));
    let version = document["version"].as_str().unwrap();
    (
        json!({"name":document["name"],"version":version,"dist":{"tarball":format!("https://registry.npmjs.org/foo/-/foo-{version}.tgz"),"integrity":integrity}}),
        bytes,
    )
}
fn published(document: Value) -> Archive {
    let (metadata, bytes) = archive(document);
    Archive::verify(&metadata, &bytes, None).unwrap()
}
fn lock(version: u64, old: &Archive) -> Lockfile {
    let entry =
        json!({"version":old.version.to_string(),"resolved":old.url,"integrity":old.integrity});
    let mut document = json!({"lockfileVersion":version});
    if version <= 2 {
        document["dependencies"] = json!({"foo":entry.clone()});
    }
    if version >= 2 {
        document["packages"] =
            json!({"":{"dependencies":{"foo":"^1.2.0"}},"node_modules/foo":entry});
    }
    Lockfile::parse(
        &serde_json::to_vec(&json!({"dependencies":{"foo":"^1.2.0"}})).unwrap(),
        &serde_json::to_vec(&document).unwrap(),
    )
    .unwrap()
}
#[test]
fn native_builds_hidden_bundles_and_nonregular_entries_require_manual_review() {
    for (path, kind) in [
        ("package/binding.gyp", tar::EntryType::Regular),
        (
            "package/node_modules/hidden/index.js",
            tar::EntryType::Regular,
        ),
        ("package/link", tar::EntryType::Symlink),
        ("package/hardlink", tar::EntryType::Link),
        ("package/device", tar::EntryType::Char),
        ("package/fifo", tar::EntryType::Fifo),
    ] {
        let (metadata, bytes) =
            archive_extra(json!({"name":"foo","version":"1.2.3"}), Some((path, kind)));
        assert!(Archive::verify(&metadata, &bytes, None).is_err(), "{path}");
    }
    let (metadata, bytes) = archive_extra(
        json!({"name":"foo","version":"1.2.3"}),
        Some(("package/README.md", tar::EntryType::Regular)),
    );
    Archive::verify(&metadata, &bytes, None).unwrap();
}
#[test]
fn all_lock_versions_read_but_only_modern_locks_update() {
    for version in 1..=3 {
        let old = published(json!({"name":"foo","version":"1.2.3"}));
        let new = published(json!({"name":"foo","version":"1.2.4"}));
        let parsed = lock(version, &old);
        assert_eq!(parsed.queries(), vec![("foo".into(), "1.2.3".into())]);
        if version == 1 {
            assert!(parsed.patch("node_modules/foo", &old, &new).is_err());
            continue;
        }
        let patch = parsed.patch("node_modules/foo", &old, &new).unwrap();
        let manifest: Value = serde_json::from_slice(&patch.manifest).unwrap();
        assert_eq!(manifest["dependencies"]["foo"], "^1.2.4");
        let updated = Lockfile::parse(&patch.manifest, &patch.lock).unwrap();
        assert_eq!(updated.packages[0].version, new.version);
    }
}
#[test]
fn formatted_output_is_bounded_before_allocating_an_expanded_document() {
    assert!(formatted(&json!({"nested":{"unknown":"value"}}), 16).is_err());
    let bytes = formatted(&json!({"a":1}), 128).unwrap();
    assert!(bytes.ends_with(b"\n"));
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"a":1})
    );
}
#[test]
fn registry_urls_and_package_names_are_not_arbitrary_targets() {
    assert!(dependency_name("@scope/pkg"));
    assert!(dependency_name("foo-bar"));
    assert!(dependency_name("Foo"));
    assert!(!crate::npm::package_name("Foo"));
    for name in ["../x", "@scope/a/b", "_private", ""] {
        assert!(!dependency_name(name));
    }
    for url in [
        "http://registry.npmjs.org/x",
        "https://registry.npmjs.org.evil/x",
        "https://user@registry.npmjs.org/x",
        "https://registry.npmjs.org:8443/x",
        "https://registry.npmjs.org/x?q=1",
    ] {
        assert!(!registry_url(url));
    }
}
#[test]
fn tarball_digest_identity_and_install_scripts_are_verified() {
    let (metadata, bytes) = archive(json!({"name":"foo","version":"1.2.3"}));
    let mut tampered = bytes.clone();
    tampered[10] ^= 1;
    assert!(Archive::verify(&metadata, &tampered, None).is_err());
    assert!(Archive::verify(&metadata, &bytes, Some("sha512-invalid")).is_err());
    let mut wrong = metadata.clone();
    wrong["name"] = json!("other");
    assert!(Archive::verify(&wrong, &bytes, None).is_err());
    for hook in ["preinstall", "install", "postinstall", "prepare"] {
        let (metadata, bytes) =
            archive(json!({"name":"foo","version":"1.2.3","scripts":{hook:"do not execute"}}));
        assert!(Archive::verify(&metadata, &bytes, None).is_err());
    }
}
#[test]
fn dependency_graph_license_and_nonpatch_changes_require_manual_review() {
    for document in [
        json!({"name":"foo","version":"1.3.0"}),
        json!({"name":"foo","version":"1.2.4","dependencies":{"extra":"*"}}),
        json!({"name":"foo","version":"1.2.4","peerDependencies":{"extra":"*"}}),
        json!({"name":"foo","version":"1.2.4","license":"different"}),
    ] {
        let old = published(json!({"name":"foo","version":"1.2.3"}));
        let new = published(document);
        assert!(lock(3, &old).patch("node_modules/foo", &old, &new).is_err());
    }
}
#[test]
fn inconsistent_root_and_v2_mirrors_are_not_rewritten() {
    for version in [2, 3] {
        let old = published(json!({"name":"foo","version":"1.2.3"}));
        let new = published(json!({"name":"foo","version":"1.2.4"}));
        let mut parsed = lock(version, &old);
        parsed.document["packages"][""]["dependencies"]["foo"] = json!("^0.1.0");
        assert!(parsed.patch("node_modules/foo", &old, &new).is_err());
    }
    let old = published(json!({"name":"foo","version":"1.2.3"}));
    let new = published(json!({"name":"foo","version":"1.2.4"}));
    let mut parsed = lock(2, &old);
    parsed.document["dependencies"]["foo"]["version"] = json!("1.2.2");
    assert!(parsed.patch("node_modules/foo", &old, &new).is_err());
}
#[test]
fn unsupported_sources_and_workspace_links_are_counted_not_fabricated() {
    let parsed=Lockfile::parse(b"{}",&serde_json::to_vec(&json!({"lockfileVersion":3,"packages":{"":{},"node_modules/workspace":{"link":true,"resolved":"packages/workspace"},"node_modules/git":{"version":"git+ssh://private"},"node_modules/foo":{"version":"1.2.3","resolved":"https://registry.npmjs.org/foo/-/foo-1.2.3.tgz"}}})).unwrap()).unwrap();
    assert_eq!(parsed.unsupported, 2);
    assert_eq!(parsed.packages.len(), 1);
    assert!(Lockfile::parse(b"{}", br#"{"lockfileVersion":4}"#).is_err());
    assert!(Lockfile::parse(b"{}", b"not JSON").is_err());
}
#[test]
fn missing_declared_packages_and_workspaces_never_claim_complete_inventory() {
    let old = published(json!({"name":"foo","version":"1.2.3"}));
    let new = published(json!({"name":"foo","version":"1.2.4"}));
    for version in [1, 2, 3] {
        let mut parsed = lock(version, &old);
        if version == 1 {
            parsed.document["dependencies"] = json!({});
        } else {
            parsed.document["packages"] = json!({"":{"dependencies":{"foo":"^1.2.0"}}});
        }
        let incomplete = Lockfile::parse(
            &serde_json::to_vec(&parsed.manifest).unwrap(),
            &serde_json::to_vec(&parsed.document).unwrap(),
        )
        .unwrap();
        assert_eq!(incomplete.unsupported, 1);
        assert!(incomplete.packages.is_empty());
        let mut parsed = lock(version, &old);
        parsed.manifest["workspaces"] = json!(["packages/*"]);
        let workspace = Lockfile::parse(
            &serde_json::to_vec(&parsed.manifest).unwrap(),
            &serde_json::to_vec(&parsed.document).unwrap(),
        )
        .unwrap();
        assert!(workspace.unsupported > 0);
        assert!(workspace.patch("node_modules/foo", &old, &new).is_err());
    }
}
#[test]
fn patches_preserve_installed_dependency_and_peer_constraints() {
    let old = published(json!({"name":"foo","version":"1.2.3"}));
    let new = published(json!({"name":"foo","version":"1.2.4"}));
    // npm v1 locks are never patched, so only v2 and v3 entries are checked.
    for (version, field) in [(2, "dependencies"), (3, "peerDependencies")] {
        for (requirement, compatible) in [
            ("1.2.3", false),
            ("=1.2.3", false),
            ("^1.2.0", true),
            ("unknown", false),
        ] {
            let mut parsed = lock(version, &old);
            let mut entry = json!({"version":"1.0.0","resolved":"https://registry.npmjs.org/bar/-/bar-1.0.0.tgz"});
            entry[field] = json!({"foo":requirement});
            parsed.document["packages"]["node_modules/bar"] = entry;
            let parsed = Lockfile::parse(
                &serde_json::to_vec(&parsed.manifest).unwrap(),
                &serde_json::to_vec(&parsed.document).unwrap(),
            )
            .unwrap();
            assert_eq!(
                parsed.patch("node_modules/foo", &old, &new).is_ok(),
                compatible,
                "{version}: {requirement}"
            );
        }
    }
}

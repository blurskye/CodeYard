use super::*;
use crate::repositories::imports::archive::read;
use crate::repositories::imports::archive::refs;
use crate::repositories::imports::archive::Records;
use crate::repositories::imports::archive::MAX_RECORDS;
use crate::repositories::migration_archive::MigrationArchive;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::json;

fn repository(name: &str, id: i64) -> Value {
    json!({"id":id,"full_name":name,"html_url":format!("https://source.test/{name}"),"url":format!("https://source.test/api/v3/repos/{name}"),"issues_url":format!("https://source.test/api/v3/repos/{name}/issues{{/number}}")})
}

#[test]
fn reads_records_refs_and_attachments_without_trusting_git_configuration() {
    let mut writer = MigrationArchive::default();
    writer
        .records("repository", vec![repository("owner/project", 1)])
        .unwrap();
    writer.records("issue", vec![json!({"id":5,"number":7,"title":"Original issue","repository":"https://source.test/owner/project"})]).unwrap();
    let sha = "a".repeat(40);
    for (path, data) in [
        ("HEAD", "ref: refs/heads/trunk\n".to_owned()),
        ("refs/heads/trunk", format!("{sha}\n")),
        (
            "packed-refs",
            format!("# pack-refs with: peeled\n{sha} refs/tags/v1\n^{sha}\n"),
        ),
        (
            "config",
            "[remote \"origin\"]\nurl=https://do-not-copy.test/\n".to_owned(),
        ),
        ("hooks/post-update", "do not execute".to_owned()),
        ("objects/info/alternates", "/do/not/read".to_owned()),
    ] {
        writer
            .append(
                &format!("repositories/owner/project.git/{path}"),
                data.as_bytes(),
            )
            .unwrap();
    }
    writer
        .append("attachments/release-assets/1", b"original asset")
        .unwrap();
    let source = read(&writer.finish().unwrap(), None).unwrap();
    assert_eq!(source.repository["full_name"], "owner/project");
    assert_eq!(source.records["issue"][0]["title"], "Original issue");
    assert_eq!(source.git_files.len(), 3);
    assert!(!source.git_files.contains_key("config"));
    assert_eq!(
        source.attachments["attachments/release-assets/1"],
        b"original asset"
    );
    let (refs, head) = refs(&source).unwrap();
    assert_eq!(head.as_deref(), Some("trunk"));
    assert_eq!(refs["refs/heads/trunk"], sha);
    assert_eq!(refs["refs/tags/v1"], sha);
}

#[test]
fn selection_does_not_mix_private_repositories() {
    let mut writer = MigrationArchive::default();
    writer
        .records(
            "repository",
            vec![repository("owner/one", 1), repository("owner/two", 2)],
        )
        .unwrap();
    writer.records("issue", vec![
        json!({"id":1,"repository":"https://source.test/owner/one","title":"One"}),
        json!({"id":2,"repository":"https://source.test/owner/two","title":"Two"}),
        json!({"id":3,"repository":"https://source.test/owner/one-more","title":"Not a prefix match"}),
    ]).unwrap();
    writer.records("issue_comment", vec![
        json!({"id":1,"issue_url":"https://source.test/api/v3/repos/owner/one/issues/7","body":"One"}),
        json!({"id":2,"issue_url":"https://source.test/api/v3/repos/owner/two/issues/7","body":"Two"}),
    ]).unwrap();
    writer.records("release_asset", vec![
        json!({"id":1,"release":"https://source.test/owner/one/releases/1","archive_path":"attachments/one"}),
        json!({"id":2,"release":"https://source.test/owner/two/releases/2","archive_path":"attachments/two"}),
    ]).unwrap();
    writer.append("attachments/one", b"One").unwrap();
    writer.append("attachments/two", b"Two").unwrap();
    let bytes = writer.finish().unwrap();
    assert!(read(&bytes, None).is_err());
    assert!(read(&bytes, Some("missing/project")).is_err());
    let source = read(&bytes, Some("OWNER/ONE")).unwrap();
    assert_eq!(source.records["issue"].len(), 1);
    assert_eq!(source.records["issue"][0]["title"], "One");
    assert_eq!(source.records["issue_comment"].len(), 1);
    assert_eq!(source.records["issue_comment"][0]["body"], "One");
    assert_eq!(source.attachments.len(), 1);
    assert_eq!(source.attachments["attachments/one"], b"One");
}

fn unsafe_archive(path: &str, link: bool) -> Vec<u8> {
    let mut writer = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    let mut header = tar::Header::new_gnu();
    header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
    header.set_mode(0o644);
    header.set_size(0);
    if link {
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_link_name("/outside").unwrap();
    }
    header.set_cksum();
    writer.append(&header, std::io::empty()).unwrap();
    writer.into_inner().unwrap().finish().unwrap()
}

#[test]
fn rejects_links_traversal_duplicates_invalid_records_and_refs() {
    for bytes in [
        unsafe_archive("../outside", false),
        unsafe_archive("/outside", false),
        unsafe_archive("link", true),
    ] {
        assert!(read(&bytes, None).is_err());
    }
    let mut writer = MigrationArchive::default();
    writer
        .records("repository", vec![repository("owner/project", 1)])
        .unwrap();
    writer
        .append("repositories/owner/project.git/HEAD", b"first")
        .unwrap();
    writer
        .append("repositories/owner/project.git/HEAD", b"second")
        .unwrap();
    assert!(read(&writer.finish().unwrap(), None).is_err());
    assert!(serde_json::from_slice::<Records>(b"[null]").is_err());
    let records = serde_json::to_vec(&vec![json!({}); MAX_RECORDS + 1]).unwrap();
    assert!(serde_json::from_slice::<Records>(&records).is_err());
    let mut writer = MigrationArchive::default();
    writer
        .records("repository", vec![repository("owner/project", 1)])
        .unwrap();
    writer
        .append(
            "repositories/owner/project.git/refs/heads/trunk",
            b"not-a-sha\n",
        )
        .unwrap();
    let source = read(&writer.finish().unwrap(), None).unwrap();
    assert!(refs(&source).is_err());
    let mut writer = MigrationArchive::default();
    writer
        .records("repository", vec![repository("owner/project", 1)])
        .unwrap();
    let bytes = writer.finish().unwrap();
    let mut concatenated = bytes.clone();
    concatenated.extend(unsafe_archive("../outside", false));
    assert!(read(&concatenated, None).is_err());
    let mut trailing = bytes.clone();
    trailing.extend_from_slice(b"not a gzip member");
    assert!(read(&trailing, None).is_err());
    let mut bytes = bytes;
    let crc = bytes.len() - 8;
    bytes[crc] ^= 1;
    assert!(read(&bytes, None).is_err());
}

#[tokio::test]
async fn repacks_loose_git_history_and_tags_without_source_configuration() {
    use std::io::Write;
    let directory = tempfile::tempdir().unwrap();
    let original = git2::Repository::init_bare(directory.path()).unwrap();
    original
        .config()
        .unwrap()
        .set_str(
            "remote.origin.url",
            "https://credential:do-not-copy@source.test/repo",
        )
        .unwrap();
    let blob = original.blob(b"original file\n").unwrap();
    let mut builder = original.treebuilder(None).unwrap();
    builder.insert("README.md", blob, 0o100644).unwrap();
    let tree = original.find_tree(builder.write().unwrap()).unwrap();
    let signature = git2::Signature::new(
        "Original author",
        "original@example.test",
        &git2::Time::new(1_500_000_000, 0),
    )
    .unwrap();
    let first = original
        .commit(
            Some("refs/heads/trunk"),
            &signature,
            &signature,
            "first",
            &tree,
            &[],
        )
        .unwrap();
    let first_commit = original.find_commit(first).unwrap();
    let tip = original
        .commit(
            Some("refs/heads/trunk"),
            &signature,
            &signature,
            "second",
            &tree,
            &[&first_commit],
        )
        .unwrap();
    let tag = original
        .tag(
            "v1",
            &original.find_object(tip, None).unwrap(),
            &signature,
            "original annotated tag",
            false,
        )
        .unwrap();
    original
        .reference("refs/pull/7/head", tip, true, "fixture")
        .unwrap();
    original.set_head("refs/heads/trunk").unwrap();
    let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for name in ["HEAD", "config"] {
        tar.append_path_with_name(directory.path().join(name), name)
            .unwrap();
    }
    for name in ["objects", "refs"] {
        tar.append_dir_all(name, directory.path().join(name))
            .unwrap();
    }
    let bytes = tar.into_inner().unwrap().finish().unwrap();
    let mut writer = MigrationArchive::default();
    writer
        .records("repository", vec![repository("owner/project", 1)])
        .unwrap();
    writer.repository("owner/project", &bytes).unwrap();
    let source = read(&writer.finish().unwrap(), None).unwrap();
    assert!(!source.git_files.contains_key("config"));
    let prepared = crate::repositories::imports::archive::git::prepare(&source)
        .await
        .unwrap();
    assert_eq!(prepared.default_branch.as_deref(), Some("trunk"));
    assert_eq!(prepared.refs["refs/heads/trunk"], tip.to_string());
    assert_eq!(prepared.refs["refs/pull/7/head"], tip.to_string());
    assert_eq!(prepared.refs["refs/tags/v1"], tag.to_string());
    let destination = tempfile::tempdir().unwrap();
    let restored = git2::Repository::init_bare(destination.path()).unwrap();
    let odb = restored.odb().unwrap();
    let mut pack = odb.packwriter().unwrap();
    pack.write_all(&prepared.pack).unwrap();
    pack.commit().unwrap();
    assert_eq!(
        restored.find_commit(tip).unwrap().parent_id(0).unwrap(),
        first
    );
    assert_eq!(
        restored
            .find_commit(first)
            .unwrap()
            .author()
            .when()
            .seconds(),
        1_500_000_000
    );
    assert_eq!(
        restored.find_blob(blob).unwrap().content(),
        b"original file\n"
    );
    assert_eq!(
        restored.find_tag(tag).unwrap().message().unwrap(),
        Some("original annotated tag")
    );
    assert!(restored
        .config()
        .unwrap()
        .get_string("remote.origin.url")
        .is_err());
}

#[tokio::test]
async fn refuses_a_missing_object_instead_of_publishing_partial_history() {
    let mut writer = MigrationArchive::default();
    writer
        .records("repository", vec![repository("owner/project", 1)])
        .unwrap();
    writer
        .append(
            "repositories/owner/project.git/refs/heads/trunk",
            "a".repeat(40).as_bytes(),
        )
        .unwrap();
    let source = read(&writer.finish().unwrap(), None).unwrap();
    assert!(crate::repositories::imports::archive::git::prepare(&source)
        .await
        .is_err());
    assert!(crate::repositories::imports::archive::git::prepare(&source)
        .await
        .is_err());
}

use super::*;

fn commit(
    repo: &Repository,
    parent: Option<git2::Oid>,
    content: &[u8],
    message: &str,
    time: i64,
) -> git2::Oid {
    let signature =
        git2::Signature::new("Test", "test@example.invalid", &git2::Time::new(time, 0)).unwrap();
    let mut builder = repo.treebuilder(None).unwrap();
    builder
        .insert("file.txt", repo.blob(content).unwrap(), 0o100644)
        .unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let parent = parent.map(|id| repo.find_commit(id).unwrap());
    repo.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        message,
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

#[tokio::test]
async fn tar_gz_limits_the_compressed_response_not_the_tar_stream() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(root.path()).unwrap();
    let data = vec![b'x'; 40 * 1024 * 1024];
    let head = commit(&repo, None, &data, "compressible", 1_700_000_000);
    let bytes = archive(
        root.path(),
        head,
        "demo",
        ArchiveFormat::TarGz,
        32 * 1024 * 1024,
    )
    .await
    .unwrap();
    let mut decoded = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::GzDecoder::new(bytes.as_slice()),
        &mut decoded,
    )
    .unwrap();
    assert!(decoded.len() > data.len());
    assert!(decoded.iter().filter(|byte| **byte == b'x').count() >= data.len());
    assert!(archive(root.path(), head, "demo", ArchiveFormat::TarGz, 16)
        .await
        .unwrap_err()
        .is::<process::OutputLimitExceeded>());
}

#[tokio::test]
async fn path_history_filters_dates_without_pruning_newer_ancestors() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(directory.path()).unwrap();
    let first = commit(&repo, None, b"one", "newer parent", 1_700_000_000);
    commit(&repo, Some(first), b"two", "older child", 1_600_000_000);
    let filter = CommitFilter {
        since: Some(1_650_000_000),
        ..Default::default()
    };
    let history = list_path_commits(directory.path(), "main", "file.txt", 0, 10, &filter)
        .await
        .unwrap();
    assert_eq!(
        history.iter().map(|c| c.sha.as_str()).collect::<Vec<_>>(),
        [first.to_string()]
    );
}

#[tokio::test]
async fn range_and_path_history_share_the_commit_page_budget() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(directory.path()).unwrap();
    let base = commit(&repo, None, b"base", "base", 1_700_000_000);
    let mut head = base;
    for index in 0..8 {
        head = commit(
            &repo,
            Some(head),
            index.to_string().as_bytes(),
            &"\"".repeat(200 * 1024),
            1_700_000_001 + index,
        );
    }
    let error = list_range_commits(directory.path(), &base.to_string(), &head.to_string(), 10)
        .await
        .unwrap_err();
    assert!(error.is::<ObjectTooLarge>());
    let error = list_path_commits(
        directory.path(),
        "main",
        "file.txt",
        0,
        10,
        &Default::default(),
    )
    .await
    .unwrap_err();
    assert!(error.is::<ObjectTooLarge>());
}

#[tokio::test]
async fn range_history_prunes_excluded_ancestors_and_checks_returned_objects() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(directory.path()).unwrap();
    let huge = commit(
        &repo,
        None,
        b"one",
        &"x".repeat(MAX_COMMIT_OBJECT_BYTES + 1),
        1_700_000_000,
    );
    let base = commit(&repo, Some(huge), b"two", "small", 1_700_000_001);
    let head = commit(&repo, Some(base), b"three", "head", 1_700_000_002);
    let commits = list_range_commits(directory.path(), &base.to_string(), &head.to_string(), 1)
        .await
        .unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].sha, head.to_string());
    // Objects we actually return still have to fit the same object-size limit.
    assert!(
        list_range_commits(directory.path(), &head.to_string(), &huge.to_string(), 1)
            .await
            .unwrap_err()
            .is::<ObjectTooLarge>()
    );
}

#[tokio::test]
async fn a_small_pr_does_not_materialize_large_base_history() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(root.path()).unwrap();
    let message = "x".repeat(240 * 1024);
    let mut parent = None;
    for index in 0..300 {
        parent = Some(commit(
            &repo,
            parent,
            b"base",
            &message,
            1_700_000_000 + index,
        ));
    }
    let base = parent.unwrap();
    let head = commit(&repo, Some(base), b"head", "one change", 1_700_000_301);
    let commits = list_range_commits(root.path(), &base.to_string(), &head.to_string(), 50)
        .await
        .unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].sha, head.to_string());
}

#[tokio::test]
async fn code_search_paginates_all_matches_in_one_file() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(directory.path()).unwrap();
    commit(
        &repo,
        None,
        "needle\n".repeat(40).as_bytes(),
        "matches",
        1_700_000_000,
    );
    let first = search_code(directory.path(), "main", "needle", 0, 31)
        .await
        .unwrap();
    let second = search_code(directory.path(), "main", "needle", 30, 31)
        .await
        .unwrap();
    assert_eq!(first.len(), 31);
    assert_eq!(second.len(), 10);
    assert_eq!(second[0].line_number, 31);
    assert_eq!(second[9].line_number, 40);
}

#[test]
fn nested_tag_names_and_large_annotations_remain_listable() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(directory.path()).unwrap();
    let id = commit(&repo, None, b"one", "base", 1_700_000_000);
    repo.reference("refs/tags/v1", id, false, "").unwrap();
    repo.reference("refs/tags/refs/tags/v1", id, false, "")
        .unwrap();
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    for index in 0..5 {
        repo.tag(
            &format!("large-{index}"),
            &repo.find_object(id, None).unwrap(),
            &signature,
            &"x".repeat(MAX_TAG_MESSAGE_BYTES),
            false,
        )
        .unwrap();
    }
    let tags = list_tags(directory.path()).unwrap();
    assert_eq!(tags.len(), 7);
    assert!(tags.iter().any(|tag| tag.name == "v1"));
    assert!(tags.iter().any(|tag| tag.name == "refs/tags/v1"));
    assert!(tags
        .iter()
        .any(|tag| tag.name.starts_with("large-") && tag.message.is_none()));
    assert!(
        tags.iter()
            .filter_map(|tag| tag.message.as_ref())
            .map(String::len)
            .sum::<usize>()
            <= MAX_TAG_RESPONSE_BYTES
    );
}

#[tokio::test]
async fn hexadecimal_branch_resolves_without_changing_existing_object_ids() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(directory.path()).unwrap();
    let id = commit(&repo, None, b"one", "base", 1_700_000_000);
    let name = "0123456789abcdef0123456789abcdef01234567";
    repo.reference(&format!("refs/heads/{name}"), id, false, "")
        .unwrap();
    repo.set_head(&format!("refs/heads/{name}")).unwrap();
    assert_eq!(
        get_commit(directory.path(), name).unwrap().sha,
        id.to_string()
    );
    assert_eq!(
        resolve_archive_revision(directory.path(), name).unwrap(),
        id
    );
    assert_eq!(
        list_tree(directory.path(), name, "").await.unwrap()[0].name,
        "file.txt"
    );
    repo.reference(&format!("refs/heads/{id}"), id, false, "")
        .unwrap();
    assert_eq!(
        get_commit(directory.path(), &id.to_string()).unwrap().sha,
        id.to_string()
    );
}

#[tokio::test]
async fn file_finder_excludes_gitlinks_but_keeps_blobs_and_symlinks() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(directory.path()).unwrap();
    let base = commit(&repo, None, b"source", "base", 1_700_000_000);
    let mut builder = repo.treebuilder(None).unwrap();
    builder.insert("submodule", base, 0o160000).unwrap();
    builder
        .insert("file.txt", repo.blob(b"source").unwrap(), 0o100644)
        .unwrap();
    builder
        .insert("link", repo.blob(b"file.txt").unwrap(), 0o120000)
        .unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    repo.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "entries",
        &tree,
        &[&repo.find_commit(base).unwrap()],
    )
    .unwrap();
    let paths = list_file_paths(directory.path(), "main", 100)
        .await
        .unwrap();
    assert_eq!(paths.paths, ["file.txt", "link"]);
    assert!(!paths.truncated);
}

#[tokio::test]
async fn branch_comparisons_share_a_deadline_and_read_all_tip_dates() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(directory.path()).unwrap();
    let first = commit(&repo, None, b"one", "base", 1_600_000_000);
    let last = commit(&repo, Some(first), b"two", "tip", 1_700_000_000);
    let heads = vec![first.to_string(); 210];
    assert!(
        branch_statuses(directory.path(), &last.to_string(), &heads, Instant::now())
            .await
            .is_err()
    );
    let statuses = branch_statuses(
        directory.path(),
        &last.to_string(),
        &heads,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(statuses.len(), 210);
    assert!(statuses.iter().all(|s| s.updated == 1_600_000_000));
    assert!(statuses[..200]
        .iter()
        .all(|s| (s.ahead, s.behind) == (0, 1)));
}

#[tokio::test]
async fn file_finder_streams_a_sorted_prefix_with_a_json_byte_budget() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(directory.path()).unwrap();
    let blob = repo.blob(b"x").unwrap();
    let mut builder = repo.treebuilder(None).unwrap();
    // The complete listing exceeds the former 16 MiB subprocess-output cap.
    for index in 0..20_000 {
        builder
            .insert(format!("{index:05}-{}", "\"".repeat(900)), blob, 0o100644)
            .unwrap();
    }
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    repo.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "paths",
        &tree,
        &[],
    )
    .unwrap();
    let found = list_file_paths(directory.path(), "main", 10_000)
        .await
        .unwrap();
    assert!(found.truncated);
    assert!(!found.paths.is_empty());
    assert!(found.paths.len() < 10_000);
    assert!(serde_json::to_vec(&found).unwrap().len() < MAX_FILE_LIST_BYTES + 128);
    for (index, path) in found.paths.iter().enumerate() {
        assert!(path.starts_with(&format!("{index:05}-")));
    }
}

#[tokio::test]
async fn blame_refuses_oversized_and_amplified_summaries() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(directory.path()).unwrap();
    let first = commit(
        &repo,
        None,
        b"one\ntwo\n",
        &"x".repeat(MAX_BLAME_SUMMARY_BYTES + 1),
        1_700_000_000,
    );
    assert!(blame_file(directory.path(), "main", "file.txt")
        .await
        .unwrap_err()
        .is::<ObjectTooLarge>());
    let content: String = (0..1000).map(|line| format!("line {line}\n")).collect();
    let second = commit(
        &repo,
        Some(first),
        content.as_bytes(),
        &"a".repeat(2048),
        1_700_000_001,
    );
    let changed: String = (0..1000)
        .map(|line| {
            if line % 2 == 0 {
                format!("changed {line}\n")
            } else {
                format!("line {line}\n")
            }
        })
        .collect();
    commit(
        &repo,
        Some(second),
        changed.as_bytes(),
        &"b".repeat(2048),
        1_700_000_002,
    );
    assert!(blame_file(directory.path(), "main", "file.txt")
        .await
        .unwrap_err()
        .is::<ObjectTooLarge>());
}

// One compressed blob a GiB larger than the default child memory limit, written with fixed
// memory and shared by the read tests.
fn oversized_history() -> &'static (tempfile::TempDir, String, String, String) {
    use sha1::{Digest, Sha1};
    use std::{io::Write, sync::OnceLock};
    static HISTORY: OnceLock<(tempfile::TempDir, String, String, String)> = OnceLock::new();
    HISTORY.get_or_init(|| {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(directory.path()).unwrap();
        let mut compressed = flate2::write::ZlibEncoder::new(
            tempfile::tempfile().unwrap(),
            flate2::Compression::fast(),
        );
        let mib = process::DEFAULT_COMMAND_MEMORY_MIB + 1024;
        let header = format!("blob {}\0", mib * 1024 * 1024);
        let mut hash = Sha1::new();
        hash.update(header.as_bytes());
        compressed.write_all(header.as_bytes()).unwrap();
        let chunk = vec![b'a'; 1024 * 1024];
        for _ in 0..mib {
            hash.update(&chunk);
            compressed.write_all(&chunk).unwrap();
        }
        let mut file = compressed.finish().unwrap();
        let id = format!("{:x}", hash.finalize());
        let objects = directory.path().join("objects").join(&id[..2]);
        std::fs::create_dir_all(&objects).unwrap();
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0)).unwrap();
        std::io::copy(
            &mut file,
            &mut std::fs::File::create(objects.join(&id[2..])).unwrap(),
        )
        .unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder
            .insert("file.txt", git2::Oid::from_str(&id).unwrap(), 0o100644)
            .unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let parent = repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "huge ancestor",
                &tree,
                &[],
            )
            .unwrap();
        let head = commit(
            &repo,
            Some(parent),
            b"retained line\n",
            "small file",
            1_700_000_000,
        );
        // A second child changes the same path, requiring the large merge base.
        let blob = repo.blob(b"other line\n").unwrap();
        let mut other_tree = repo.treebuilder(None).unwrap();
        other_tree.insert("file.txt", blob, 0o100644).unwrap();
        let other_tree = repo.find_tree(other_tree.write().unwrap()).unwrap();
        let other = repo
            .commit(
                None,
                &signature,
                &signature,
                "other child",
                &other_tree,
                &[&repo.find_commit(parent).unwrap()],
            )
            .unwrap();
        (
            directory,
            parent.to_string(),
            head.to_string(),
            other.to_string(),
        )
    })
}

#[tokio::test]
async fn blame_keeps_oversized_historical_blobs_out_of_the_server() {
    let (directory, _, _, _) = oversized_history();
    let error = blame_file(directory.path(), "main", "file.txt")
        .await
        .unwrap_err();
    assert!(error.is::<BlameRefused>(), "{error:#}");
}

#[tokio::test]
async fn search_refuses_a_blob_larger_than_its_child_memory_limit() {
    let (directory, huge, _, _) = oversized_history();
    let error = search_code(directory.path(), huge, "absent", 0, 10)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("Git could not search"),
        "{error:#}"
    );
}

#[tokio::test]
async fn diff_refuses_blobs_larger_than_its_child_memory_limit() {
    let (directory, huge, _, _) = oversized_history();
    let error = get_commit_diff(directory.path(), huge).await.unwrap_err();
    assert!(
        error.to_string().contains("Failed to get diff "),
        "{error:#}"
    );
}

#[tokio::test]
async fn merge_and_replay_refuse_a_base_larger_than_the_child_memory_limit() {
    let (directory, huge, head, other) = oversized_history();
    let person = checked::Person {
        name: "Test",
        email: "test@example.invalid",
        date: None,
    };
    let error = checked::prepare_merge(directory.path(), head, other, "merge", &person)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("Git could not merge"),
        "{error:#}"
    );
    let error = replay_commits(directory.path(), huge, head, other, &person)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("Git could not merge"),
        "{error:#}"
    );
}

#[tokio::test]
async fn merge_verifies_an_explicit_branch_even_when_its_name_looks_like_a_pull_ref() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(directory.path()).unwrap();
    let base = commit(&repo, None, b"one", "base", 1_700_000_000);
    let head = commit(&repo, Some(base), b"two", "head", 1_700_000_001);
    repo.reference("refs/heads/main", base, true, "").unwrap();
    let reference = "refs/heads/refs/pull/42/head";
    repo.reference(reference, head, false, "").unwrap();
    publish_merge(
        directory.path(),
        &[(reference, &head.to_string())],
        "main",
        &base.to_string(),
        &head.to_string(),
    )
    .await
    .unwrap();
    assert_eq!(repo.refname_to_id("refs/heads/main").unwrap(), head);
}

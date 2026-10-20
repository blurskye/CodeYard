use super::*;

fn add(repo: &Repository, parent: Option<git2::Oid>, message: &str, name: &str) -> git2::Oid {
    let signature = git2::Signature::now(name, "test@example.invalid").unwrap();
    let mut builder = repo.treebuilder(None).unwrap();
    builder
        .insert("a\nb.txt", repo.blob(message.as_bytes()).unwrap(), 0o100644)
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
async fn declared_commit_encoding_is_shared_by_reads_and_message_search() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(root.path()).unwrap();
    let tree = repo.treebuilder(None).unwrap().write().unwrap();
    let mut raw = format!("tree {tree}\nauthor Test <test@example.invalid> 1700000000 +0000\ncommitter Test <test@example.invalid> 1700000000 +0000\nencoding ISO-8859-1\n\ncaf").into_bytes();
    raw.extend_from_slice(b"\xe9\n");
    let id = repo.odb().unwrap().write(ObjectType::Commit, &raw).unwrap();
    repo.reference("refs/heads/main", id, true, "fixture")
        .unwrap();
    assert_eq!(get_commit(root.path(), "main").unwrap().message, "café\n");
    let filter = CommitFilter {
        message: Some("CAFÉ".into()),
        ..Default::default()
    };
    assert_eq!(
        list_commits(
            root.path(),
            "main",
            0,
            10,
            &filter,
            Instant::now() + COMMIT_WALK_TIMEOUT
        )
        .unwrap()
        .len(),
        1
    );
}

#[tokio::test]
async fn path_author_matching_and_pagination_use_unicode_case_folding() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(root.path()).unwrap();
    let first = add(&repo, None, "first", "Élodie");
    let second = add(&repo, Some(first), "second", "Élodie");
    add(&repo, Some(second), "unmatched", "Other");
    let filter = CommitFilter {
        author: Some("élodie".into()),
        ..Default::default()
    };
    let page = list_path_commits(root.path(), "main", "a\nb.txt", 1, 1, &filter)
        .await
        .unwrap();
    assert_eq!(page[0].sha, first.to_string());
}

#[tokio::test]
async fn code_search_preserves_newlines_in_paths_and_ref_errors() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(root.path()).unwrap();
    add(&repo, None, "needle\nneedle\n", "Test");
    let found = search_code(root.path(), "main", "needle", 1, 1)
        .await
        .unwrap();
    assert_eq!(found[0].path, "a\nb.txt");
    assert_eq!(found[0].line_number, 2);
    assert!(search_code(root.path(), "missing", "needle", 0, 1)
        .await
        .is_err());
}

#[tokio::test]
async fn branch_statuses_cache_shared_tips_and_stop_at_the_batch_deadline() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(root.path()).unwrap();
    let first = add(&repo, None, "first", "Test");
    let mut head = first;
    for _ in 0..1100 {
        head = add(&repo, Some(head), "next", "Test");
    }
    let heads = vec![head.to_string(); 199];
    let statuses = branch_statuses(
        root.path(),
        &first.to_string(),
        &heads,
        Instant::now() + COMMIT_WALK_TIMEOUT,
    )
    .await
    .unwrap();
    assert_eq!(statuses.len(), 199);
    assert!(statuses.iter().all(|s| s.ahead == 1100 && s.behind == 0));
    assert!(branch_statuses(root.path(), "main", &heads, Instant::now())
        .await
        .unwrap_err()
        .is::<ObjectTooLarge>());
}

#[tokio::test]
async fn tree_history_keeps_latest_results_when_the_full_log_exceeds_its_budget() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(root.path()).unwrap();
    let mut head = None;
    for index in 0..5000 {
        head = Some(add(
            &repo,
            head,
            &format!("{index}:{}", "x".repeat(1000)),
            "Test",
        ));
    }
    let entries = list_tree(root.path(), "main", "").await.unwrap();
    assert_eq!(
        entries[0].last_commit.as_ref().unwrap().sha,
        head.unwrap().to_string()
    );
    // An empty tree has no changed path, so history must exhaust its budget while
    // retaining the already resolved file instead of discarding the whole result.
    let parent = repo.find_commit(head.unwrap()).unwrap();
    let mut builder = repo.treebuilder(Some(&parent.tree().unwrap())).unwrap();
    let empty = repo.treebuilder(None).unwrap().write().unwrap();
    builder.insert("empty", empty, 0o040000).unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    repo.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "empty directory",
        &tree,
        &[&parent],
    )
    .unwrap();
    let entries = list_tree(root.path(), "main", "").await.unwrap();
    let file = entries
        .iter()
        .find(|entry| entry.entry_type == "file")
        .unwrap();
    assert_eq!(
        file.last_commit.as_ref().unwrap().sha,
        head.unwrap().to_string()
    );
    assert!(entries
        .iter()
        .find(|entry| entry.name == "empty")
        .unwrap()
        .last_commit
        .is_none());
}

#[tokio::test]
async fn sidebar_does_not_load_blob_headers_but_normal_tree_keeps_sizes() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(root.path()).unwrap();
    add(&repo, None, "content", "Test");
    let normal = list_tree(root.path(), "main", "").await.unwrap();
    assert_eq!(normal[0].size, Some(7));
    let id = repo.blob(b"content").unwrap().to_string();
    std::fs::remove_file(root.path().join("objects").join(&id[..2]).join(&id[2..])).unwrap();
    let sidebar = list_sidebar_trees(root.path(), "main", &[String::new()])
        .await
        .unwrap();
    assert_eq!(sidebar[0].1[0].name, "a\nb.txt");
    assert_eq!(sidebar[0].1[0].size, None);
    assert!(list_tree(root.path(), "main", "").await.is_err());
}

#[tokio::test]
async fn new_protected_branch_checks_merges_already_reachable_from_scratch() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(root.path()).unwrap();
    let base = add(&repo, None, "base", "Test");
    let head = add(&repo, Some(base), "head", "Test");
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    let tree = repo.find_commit(head).unwrap().tree().unwrap();
    let merge = repo
        .commit(
            Some("refs/heads/scratch"),
            &signature,
            &signature,
            "merge",
            &tree,
            &[
                &repo.find_commit(base).unwrap(),
                &repo.find_commit(head).unwrap(),
            ],
        )
        .unwrap();
    assert!(
        adds_merge_commit(root.path(), None, None, &merge.to_string())
            .await
            .unwrap()
    );
    assert!(!adds_merge_commit(
        root.path(),
        None,
        Some(&merge.to_string()),
        &merge.to_string()
    )
    .await
    .unwrap());
}

#[tokio::test]
async fn squash_subjects_are_byte_bounded_even_for_zero_width_characters() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(root.path()).unwrap();
    let base = add(&repo, None, "base", "Test");
    let mut head = base;
    for index in 0..6 {
        head = add(
            &repo,
            Some(head),
            &format!("{index}a{}", "\u{301}".repeat(100_000)),
            "Test",
        );
    }
    let person = Person {
        name: "Test",
        email: "test@example.invalid",
        date: None,
    };
    let id = prepare_squash(
        root.path(),
        &head.to_string(),
        &base.to_string(),
        "Squash",
        &person,
        &person,
    )
    .await
    .unwrap();
    let info = get_commit(root.path(), &id).unwrap();
    assert!(info.message.len() < 8192);
    assert_eq!(
        info.message
            .lines()
            .filter(|line| line.starts_with("* "))
            .count(),
        6
    );
}

#[tokio::test]
async fn fork_accepts_many_long_ref_names_without_exhausting_diagnostics() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.git");
    let repo = Repository::init_bare(&source).unwrap();
    let id = add(&repo, None, "base", "Test");
    for index in 0..1000 {
        let name = format!("refs/heads/{index}/{0}/{0}/{0}/{0}/{0}", "a".repeat(175));
        repo.reference(&name, id, false, "fixture").unwrap();
    }
    let destination = root.path().join("fork.git");
    initialize_repository(&destination, Some(&source), "main")
        .await
        .unwrap();
    assert_eq!(list_refs(&destination).unwrap().len(), 1001);
}

#[tokio::test]
async fn sibling_new_branches_count_against_the_pre_push_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(root.path()).unwrap();
    let old = add(&repo, None, "old", "Test");
    let new = add(&repo, Some(old), "new", "Test");
    repo.reference("refs/heads/sibling", new, false, "published")
        .unwrap();
    assert_eq!(
        count_pushed_commits_snapshot(root.path(), None, &new.to_string(), &[])
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        count_pushed_commits_snapshot(root.path(), None, &new.to_string(), &[old.to_string()])
            .await
            .unwrap(),
        1
    );
}

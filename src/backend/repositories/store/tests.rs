//! The store against real bare repositories in temporary directories.
use super::*;
use serde::de::DeserializeOwned;

/// Sends a value through JSON and back, the way a remote store would carry it.
fn round_trip<T: Serialize + DeserializeOwned>(value: &T) -> T {
    serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
}

/// A merge commit of `head` into `base`, committed by `name` and `email`.
fn merge_request(head: &str, base: &str, message: &str, name: &str, email: &str) -> MergeRequest {
    MergeRequest {
        head_branch: "feature".into(),
        base_branch: "main".into(),
        head: head.into(),
        base: base.into(),
        message: message.into(),
        name: name.into(),
        email: email.into(),
        method: MergeMethod::Merge,
        author_name: None,
        author_email: None,
    }
}

fn signature() -> git2::Signature<'static> {
    git2::Signature::now("Tester", "tester@example.invalid").unwrap()
}

/// `base` with the file at `path` (which may be nested) set to `content`.
fn with_file(
    repo: &git2::Repository,
    base: Option<&git2::Tree>,
    path: &str,
    content: &str,
) -> git2::Oid {
    let mut builder = repo.treebuilder(base).unwrap();
    match path.split_once('/') {
        None => {
            builder
                .insert(path, repo.blob(content.as_bytes()).unwrap(), 0o100644)
                .unwrap();
        }
        Some((directory, rest)) => {
            let subtree = base
                .and_then(|tree| tree.get_name(directory))
                .map(|entry| repo.find_tree(entry.id()).unwrap());
            let id = with_file(repo, subtree.as_ref(), rest, content);
            builder.insert(directory, id, 0o040000).unwrap();
        }
    }
    builder.write().unwrap()
}

/// Commits one file change on `branch`, on top of `parent`.
fn commit(
    repo: &git2::Repository,
    branch: &str,
    parent: Option<git2::Oid>,
    path: &str,
    content: &str,
    message: &str,
) -> git2::Oid {
    let parent = parent.map(|oid| repo.find_commit(oid).unwrap());
    let base = parent.as_ref().map(|commit| commit.tree().unwrap());
    let tree = repo
        .find_tree(with_file(repo, base.as_ref(), path, content))
        .unwrap();
    let author = signature();
    repo.commit(
        Some(&format!("refs/heads/{branch}")),
        &author,
        &author,
        message,
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

/// A repository with `README.md`, then `src/lib.rs`, committed on `main`.
struct Fixture {
    _root: tempfile::TempDir,
    store: RepoStore,
    repo: git2::Repository,
    first: git2::Oid,
    second: git2::Oid,
}

struct Authority {
    _root: tempfile::TempDir,
    pool: db::DbPool,
    repo_id: i64,
    actor: i64,
}

async fn authority(fixture: &Fixture) -> Authority {
    let root = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}",
        root.path().join("authority.sqlite").display()
    );
    let pool = db::init_db(&url).await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    let actor = db::users::accounts::create_user(&pool, "store_actor", None)
        .await
        .unwrap();
    let repository = db::repositories::records::create_repository(
        &pool,
        actor.id,
        "store-tests",
        None,
        false,
        fixture.store.location(),
    )
    .await
    .unwrap();
    Authority {
        _root: root,
        pool,
        repo_id: repository.id,
        actor: actor.id,
    }
}

async fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let store = RepoStore::create(root.path(), "main", None).await.unwrap();
    let repo = git2::Repository::open_bare(store.location()).unwrap();
    let first = commit(&repo, "main", None, "README.md", "# hello\n", "first");
    let second = commit(
        &repo,
        "main",
        Some(first),
        "src/lib.rs",
        "pub fn answer() -> u32 { 42 }\n",
        "second",
    );
    Fixture {
        _root: root,
        store,
        repo,
        first,
        second,
    }
}

#[tokio::test]
async fn pull_head_publication_is_retryable_and_rejects_a_stale_expected_ref() {
    let f = fixture().await;
    let path = std::path::Path::new(f.store.location());
    let first = f.first.to_string();
    let second = f.second.to_string();

    let prepared = crate::backend::git::repository::prepare_pull_head(path, &first)
        .await
        .unwrap();
    crate::backend::git::repository::publish_pull_head(path, 1, &first, None, &prepared)
        .await
        .unwrap();
    // If the success response is lost, retrying the initial exact-CAS publish
    // recognizes the already-published target rather than treating it as stale.
    crate::backend::git::repository::publish_pull_head(path, 1, &first, None, &prepared)
        .await
        .unwrap();

    let prepared = crate::backend::git::repository::prepare_pull_head(path, &second)
        .await
        .unwrap();
    crate::backend::git::repository::publish_pull_head(path, 1, &second, Some(&first), &prepared)
        .await
        .unwrap();
    let prepared = crate::backend::git::repository::prepare_pull_head(path, &first)
        .await
        .unwrap();
    let error = crate::backend::git::repository::publish_pull_head(
        path,
        1,
        &first,
        Some(&first),
        &prepared,
    )
    .await
    .unwrap_err();
    assert!(error.is::<crate::backend::git::repository::PullHeadConflict>());
    assert_eq!(f.repo.refname_to_id("refs/pull/1/head").unwrap(), f.second);

    f.repo
        .find_reference("refs/pull/1/head")
        .unwrap()
        .delete()
        .unwrap();
    let error = crate::backend::git::repository::publish_pull_head(
        path,
        1,
        &first,
        Some(&second),
        &prepared,
    )
    .await
    .unwrap_err();
    assert!(error.is::<crate::backend::git::repository::PullHeadConflict>());
    assert!(f.repo.find_reference("refs/pull/1/head").is_err());
}

#[tokio::test]
async fn revoked_private_fetch_never_exposes_quarantined_objects() {
    let base = fixture().await;
    let source = fixture().await;
    let private_head = commit(
        &source.repo,
        "private-topic",
        Some(source.second),
        "private.txt",
        "not authorized after fetch\n",
        "private head",
    );
    let authority = authority(&base).await;
    let source_owner =
        db::users::accounts::create_user(&authority.pool, "private_source_owner", None)
            .await
            .unwrap();
    let source_record = db::repositories::records::create_repository(
        &authority.pool,
        source_owner.id,
        "private-source",
        None,
        true,
        source.store.location(),
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        &authority.pool,
        authority.actor,
        source_record.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();

    let head = private_head.to_string();
    let prepared = crate::backend::git::repository::prepare_pull_head(
        std::path::Path::new(source.store.location()),
        &head,
    )
    .await
    .unwrap();
    db::repositories::access::revoke_repo_access(
        &authority.pool,
        authority.actor,
        source_record.id,
    )
    .await
    .unwrap();
    let error = base
        .store
        .local()
        .unwrap()
        .publish_fetched_pull_head(
            &authority.pool,
            authority.repo_id,
            source_record.id,
            authority.actor,
            source.store.local().unwrap(),
            1,
            &head,
            None,
            prepared,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert!(base.repo.find_object(private_head, None).is_err());
    assert!(base.repo.find_reference("refs/pull/1/head").is_err());
}

/// A commit on `branch`, by `name`, made at Unix time `time`, that sets `path` to `content`.
/// Without a branch the commit is made but no ref moves.
fn commit_by_author(
    repo: &git2::Repository,
    branch: Option<&str>,
    parent: git2::Oid,
    name: &str,
    time: i64,
    path: &str,
    content: &str,
) -> git2::Oid {
    let parent = repo.find_commit(parent).unwrap();
    let tree = repo
        .find_tree(with_file(
            repo,
            Some(&parent.tree().unwrap()),
            path,
            content,
        ))
        .unwrap();
    let email = format!("{}@example.invalid", name.to_lowercase());
    let author = git2::Signature::new(name, &email, &git2::Time::new(time, 0)).unwrap();
    let update = branch.map(|branch| format!("refs/heads/{branch}"));
    repo.commit(update.as_deref(), &author, &author, name, &tree, &[&parent])
        .unwrap()
}

#[tokio::test]
async fn commit_walk_gives_up_at_the_deadline_its_caller_set() {
    let f = fixture().await;
    let passed = std::time::Instant::now() - std::time::Duration::from_secs(1);
    assert!(f
        .store
        .commits_until("main", 0, 10, &CommitFilter::default(), passed)
        .await
        .is_err());
    let ahead = std::time::Instant::now() + std::time::Duration::from_secs(30);
    assert!(!f
        .store
        .commits_until("main", 0, 10, &CommitFilter::default(), ahead)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn commit_history_filters_by_author_and_committer_time() {
    let f = fixture().await;
    // Two people, three commits at fixed times, on a branch that starts at the fixture's tip.
    let ada_old = commit_by_author(
        &f.repo,
        Some("filtered"),
        f.second,
        "Ada",
        1_700_000_000,
        "notes.md",
        "one\n",
    );
    let grace = commit_by_author(
        &f.repo,
        Some("filtered"),
        ada_old,
        "Grace",
        1_750_000_000,
        "notes.md",
        "two\n",
    );
    let ada_new = commit_by_author(
        &f.repo,
        Some("filtered"),
        grace,
        "Ada",
        1_800_000_000,
        "notes.md",
        "three\n",
    );
    let filter = |author: Option<&str>, since: Option<i64>, until: Option<i64>| CommitFilter {
        author: author.map(str::to_owned),
        since,
        until,
        message: None,
    };
    let shas = |commits: &[CommitInfo]| -> Vec<String> {
        commits.iter().map(|commit| commit.sha.clone()).collect()
    };
    let (ada_old, grace, ada_new) = (ada_old.to_string(), grace.to_string(), ada_new.to_string());

    // The author matches part of "Name <email>" in any case.
    let by_ada = f
        .store
        .commits("filtered", 0, 10, &filter(Some("ADA"), None, None))
        .await
        .unwrap();
    assert_eq!(shas(&by_ada), vec![ada_new.clone(), ada_old.clone()]);
    // Dates are Unix seconds on the committer time: `since` keeps newer commits, `until` older.
    let since = f
        .store
        .commits("filtered", 0, 10, &filter(None, Some(1_720_000_000), None))
        .await
        .unwrap();
    assert_eq!(shas(&since)[..2], [ada_new.clone(), grace.clone()]);
    assert!(!shas(&since).contains(&ada_old));
    let until = f
        .store
        .commits("filtered", 0, 10, &filter(None, None, Some(1_760_000_000)))
        .await
        .unwrap();
    assert_eq!(shas(&until), vec![grace.clone(), ada_old.clone()]);
    // Filters combine, and paging counts only the commits that pass them.
    let both = f
        .store
        .commits(
            "filtered",
            0,
            10,
            &filter(Some("ada"), Some(1_720_000_000), None),
        )
        .await
        .unwrap();
    assert_eq!(shas(&both), vec![ada_new.clone()]);
    let second_ada = f
        .store
        .commits("filtered", 1, 1, &filter(Some("ada"), None, None))
        .await
        .unwrap();
    assert_eq!(shas(&second_ada), vec![ada_old.clone()]);

    // A file history applies the same author and date filters.
    let grace_notes = f
        .store
        .path_commits(
            "filtered",
            "notes.md",
            0,
            10,
            &filter(Some("grace"), None, None),
        )
        .await
        .unwrap();
    assert_eq!(shas(&grace_notes), vec![grace.clone()]);
    let recent_notes = f
        .store
        .path_commits(
            "filtered",
            "notes.md",
            0,
            10,
            &filter(None, Some(1_740_000_000), None),
        )
        .await
        .unwrap();
    assert_eq!(shas(&recent_notes), vec![ada_new, grace]);
}

#[tokio::test]
async fn branch_statuses_count_divergence_from_the_default_branch() {
    let f = fixture().await;
    // `topic` starts at the first commit and adds one commit of its own.
    let topic_tip = commit(
        &f.repo,
        "topic",
        Some(f.first),
        "topic.txt",
        "topic\n",
        "topic work",
    );
    let main = f.second.to_string();
    let topic = topic_tip.to_string();
    let first = f.first.to_string();

    let statuses = f
        .store
        .branch_statuses(&main, &[topic, main.clone(), first])
        .await
        .unwrap();
    // `topic` has one commit main lacks, and main has one commit it lacks.
    assert_eq!((statuses[0].ahead, statuses[0].behind), (1, 1));
    assert_eq!(
        statuses[0].updated,
        f.repo
            .find_commit(topic_tip)
            .unwrap()
            .committer()
            .when()
            .seconds()
    );
    assert_eq!((statuses[1].ahead, statuses[1].behind), (0, 0));
    // `first` is an ancestor of main, so only main's second commit is missing from it.
    assert_eq!((statuses[2].ahead, statuses[2].behind), (0, 1));
}

#[tokio::test]
async fn reads_return_owned_values_that_survive_json() {
    let f = fixture().await;
    let (first, second) = (f.first.to_string(), f.second.to_string());
    let tagged = f.repo.find_commit(f.first).unwrap();
    f.repo
        .tag("v1", tagged.as_object(), &signature(), "release one", false)
        .unwrap();

    let refs = round_trip(&f.store.refs().await.unwrap());
    assert_eq!(refs["refs/heads/main"], second);
    assert!(refs.contains_key("refs/tags/v1"));

    let info = round_trip(&f.store.info().await.unwrap());
    assert_eq!(info.default_branch, "main");
    assert_eq!(info.branches, vec!["main"]);
    assert_eq!(round_trip(&f.store.commit_count().await.unwrap()), 2);

    let root = round_trip(&f.store.tree("main", "").await.unwrap());
    let listing: Vec<_> = root
        .iter()
        .map(|entry| (entry.name.as_str(), entry.entry_type.as_str()))
        .collect();
    assert_eq!(listing, vec![("src", "dir"), ("README.md", "file")]);
    assert_eq!(root[0].last_commit.as_ref().unwrap().sha, second);
    let nested = f.store.tree("main", "src").await.unwrap();
    assert_eq!(nested[0].path, "src/lib.rs");

    let readme = round_trip(&f.store.blob("main", "README.md").await.unwrap());
    assert_eq!(readme.content, "# hello\n");
    assert!(!readme.is_binary);
    let lib = round_trip(&f.store.blob_bytes("main", "src/lib.rs").await.unwrap());
    assert_eq!(lib.bytes, b"pub fn answer() -> u32 { 42 }\n");
    let range = f
        .store
        .blob_range(
            "main",
            "README.md",
            BlobRange::Bytes {
                start: 2,
                end: Some(4),
            },
        )
        .await
        .unwrap();
    assert_eq!(range.bytes, b"hel");
    assert_eq!((range.offset, range.size), (2, 8));
    assert_eq!(
        f.store
            .blob_range("main", "README.md", BlobRange::Suffix { length: 2 })
            .await
            .unwrap()
            .bytes,
        b"o\n"
    );
    let beyond = f
        .store
        .blob_range(
            "main",
            "README.md",
            BlobRange::Window {
                offset: 99,
                length: 10,
            },
        )
        .await
        .unwrap();
    assert!(beyond.bytes.is_empty());
    assert_eq!(beyond.offset, 8);
    assert!(f
        .store
        .blob_range(
            "main",
            "README.md",
            BlobRange::Bytes {
                start: 8,
                end: None,
            },
        )
        .await
        .unwrap_err()
        .is_range_not_satisfiable());
    let readme_id = f
        .repo
        .find_commit(f.first)
        .unwrap()
        .tree()
        .unwrap()
        .get_name("README.md")
        .unwrap()
        .id()
        .to_string();
    assert_eq!(f.store.blob_by_id(&readme_id).await.unwrap(), b"# hello\n");

    let commits = round_trip(
        &f.store
            .commits("main", 0, 10, &Default::default())
            .await
            .unwrap(),
    );
    let shas: Vec<_> = commits.iter().map(|commit| commit.sha.as_str()).collect();
    assert_eq!(shas, vec![second.as_str(), first.as_str()]);
    let next = f
        .store
        .commits("main", 1, 1, &Default::default())
        .await
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].sha, first);
    assert!(f
        .store
        .commits(
            "main",
            MAX_COMMIT_OFFSET,
            MAX_COMMIT_PAGE_SIZE,
            &Default::default(),
        )
        .await
        .unwrap()
        .is_empty());
    assert!(f
        .store
        .commits("main", MAX_COMMIT_OFFSET + 1, 1, &Default::default())
        .await
        .unwrap_err()
        .is_response_too_large());
    assert!(f
        .store
        .commits("main", 0, MAX_COMMIT_PAGE_SIZE + 1, &Default::default())
        .await
        .unwrap_err()
        .is_response_too_large());

    // A path lists only the commits that changed it, newest first. A traversal part names no
    // file, so it lists none.
    let readme = f
        .store
        .path_commits("main", "README.md", 0, 10, &Default::default())
        .await
        .unwrap();
    let readme_shas: Vec<_> = readme.iter().map(|commit| commit.sha.as_str()).collect();
    assert_eq!(readme_shas, vec![first.as_str()]);
    let library = f
        .store
        .path_commits("main", "src", 0, 10, &Default::default())
        .await
        .unwrap();
    let library_shas: Vec<_> = library.iter().map(|commit| commit.sha.as_str()).collect();
    assert_eq!(library_shas, vec![second.as_str()]);
    assert!(f
        .store
        .path_commits("main", "../README.md", 0, 10, &Default::default())
        .await
        .unwrap()
        .is_empty());
    let latest = round_trip(&f.store.commit(&second).await.unwrap());
    assert_eq!(latest.message.trim(), "second");
    assert_eq!(latest.parents, vec![first.clone()]);

    let change = round_trip(&f.store.commit_diff(&second).await.unwrap());
    assert_eq!(change.changed_files, 1);
    assert_eq!(change.files[0].filename, "src/lib.rs");
    assert_eq!(change.files[0].status, "added");
    let range = round_trip(&f.store.diff(&first, &second).await.unwrap());
    assert_eq!(range.files[0].filename, "src/lib.rs");
    assert_eq!(
        f.store.changed_paths(&first, &second).await.unwrap(),
        ["src/lib.rs"]
    );
    assert_eq!(
        f.store
            .changed_paths("0000000000000000000000000000000000000000", &first)
            .await
            .unwrap(),
        ["README.md"]
    );

    let tags = round_trip(&f.store.tags().await.unwrap());
    assert_eq!(tags[0].name, "v1");
    assert!(tags[0].message.as_deref().unwrap().contains("release one"));

    let archive = f
        .store
        .archive("main", "demo", crate::backend::git::ArchiveFormat::Zip)
        .await
        .unwrap();
    assert!(archive.starts_with(b"PK"));
    let languages = round_trip(&f.store.languages("main").await.unwrap());
    let names: Vec<_> = languages.iter().map(|l| l.language.as_str()).collect();
    assert!(names.contains(&"Rust") && names.contains(&"Markdown"));
    let contributors = round_trip(&f.store.contributors().await.unwrap());
    assert_eq!(contributors[0].email, "tester@example.invalid");
    assert_eq!(contributors[0].commits, 2);
    assert!(f.store.is_ancestor(&first, &second).await.unwrap());
    assert!(!f.store.is_ancestor(&second, &first).await.unwrap());
    assert!(f.store.exists().await);
    assert!(f.store.size_bytes().await.unwrap() > 0);
}

#[tokio::test]
async fn changed_path_reads_do_not_materialize_large_patch_bodies() {
    let f = fixture().await;
    let content = "x".repeat(MAX_BLOB_RESPONSE_BYTES + 1);
    let head = commit(
        &f.repo,
        "large-change",
        Some(f.second),
        "large.txt",
        &content,
        "large change",
    );
    drop(content);

    assert_eq!(
        f.store
            .changed_paths(&f.second.to_string(), &head.to_string())
            .await
            .unwrap(),
        ["large.txt"]
    );
    assert!(f
        .store
        .blob_bytes("large-change", "large.txt")
        .await
        .unwrap_err()
        .is_response_too_large());
    assert!(f
        .store
        .blob("large-change", "large.txt")
        .await
        .unwrap_err()
        .is_response_too_large());
}

#[tokio::test]
async fn archives_resolve_revisions_before_spawning_git() {
    let f = fixture().await;
    let path = std::path::Path::new(f.store.location());
    let head = std::fs::read(path.join("HEAD")).unwrap();
    for revision in ["--output=HEAD", "--output=unexpected.zip", "-oHEAD"] {
        assert!(f
            .store
            .archive(revision, "demo", crate::backend::git::ArchiveFormat::Zip)
            .await
            .is_err());
        assert_eq!(std::fs::read(path.join("HEAD")).unwrap(), head);
        assert!(!path.join("unexpected.zip").exists());
    }
    let target = f.repo.find_commit(f.first).unwrap();
    f.repo
        .tag("v1", target.as_object(), &signature(), "release", false)
        .unwrap();
    for revision in ["main", "v1", &f.second.to_string()] {
        assert!(f
            .store
            .archive(revision, "demo", crate::backend::git::ArchiveFormat::Zip)
            .await
            .unwrap()
            .starts_with(b"PK"));
    }
    // An expression is not a ref or an object ID, so it is refused even for a directory.
    assert!(f
        .store
        .archive("main:src", "demo", crate::backend::git::ArchiveFormat::Zip)
        .await
        .is_err());
    assert_eq!(std::fs::read(path.join("HEAD")).unwrap(), head);
}

#[tokio::test]
async fn valid_utf8_branch_and_tag_names_are_preserved() {
    let f = fixture().await;
    let branch = "føø/分支";
    let tag = "rélease-标签";
    f.repo
        .reference(&format!("refs/heads/{branch}"), f.second, false, "test")
        .unwrap();
    f.repo
        .reference(&format!("refs/tags/{tag}"), f.first, false, "test")
        .unwrap();

    let refs = f.store.refs().await.unwrap();
    assert_eq!(refs[&format!("refs/heads/{branch}")], f.second.to_string());
    assert_eq!(refs[&format!("refs/tags/{tag}")], f.first.to_string());
    assert!(f
        .store
        .info()
        .await
        .unwrap()
        .branches
        .contains(&branch.to_owned()));
    assert!(!f.store.tree(branch, "").await.unwrap().is_empty());
    assert_eq!(f.store.commit_diff(tag).await.unwrap().changed_files, 1);
}

#[tokio::test]
async fn tree_reads_resolve_option_looking_refs() {
    let f = fixture().await;
    let path = std::path::Path::new(f.store.location());
    let head = std::fs::read(path.join("HEAD")).unwrap();
    for (namespace, revision, target, count) in [
        ("tags", "--output=root-history", f.first, 1),
        ("heads", "--output=head-history", f.second, 2),
    ] {
        let sentinel = path.join(revision.strip_prefix("--output=").unwrap());
        std::fs::write(&sentinel, b"keep this file\n").unwrap();
        f.repo
            .reference(
                &format!("refs/{namespace}/{revision}"),
                target,
                false,
                "test",
            )
            .unwrap();

        let tree = f.store.tree(revision, "").await.unwrap();
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep this file\n");
        assert_eq!(std::fs::read(path.join("HEAD")).unwrap(), head);
        assert_eq!(tree.len(), count);
        let readme = tree.iter().find(|entry| entry.name == "README.md").unwrap();
        assert_eq!(
            readme.last_commit.as_ref().unwrap().sha,
            f.first.to_string()
        );
    }
}

#[tokio::test]
async fn commits_between_lists_only_the_commits_the_head_adds() {
    let f = fixture().await;
    let base = f.first.to_string();
    let head = f.second.to_string();
    let added = f.store.commits_between(&base, &head, 10).await.unwrap();
    assert_eq!(
        added
            .iter()
            .map(|commit| commit.sha.as_str())
            .collect::<Vec<_>>(),
        [head.as_str()]
    );
    assert!(f
        .store
        .commits_between(&head, &head, 10)
        .await
        .unwrap()
        .is_empty());
    assert!(f
        .store
        .commits_between(&base, &head, 0)
        .await
        .unwrap()
        .is_empty());
    assert!(f
        .store
        .commits_between(&base, &head, MAX_COMMIT_PAGE_SIZE + 1)
        .await
        .is_err());
}

#[tokio::test]
async fn commit_diffs_resolve_option_looking_refs() {
    let f = fixture().await;
    let path = std::path::Path::new(f.store.location());
    let head = std::fs::read(path.join("HEAD")).unwrap();
    for (namespace, revision, target) in [
        ("tags", "--output=root-diff", f.first),
        ("heads", "--output=head-diff", f.second),
    ] {
        let sentinel = path.join(revision.strip_prefix("--output=").unwrap());
        std::fs::write(&sentinel, b"keep this file\n").unwrap();
        f.repo
            .reference(
                &format!("refs/{namespace}/{revision}"),
                target,
                false,
                "test",
            )
            .unwrap();
        let expected = f.store.commit_diff(&target.to_string()).await.unwrap();
        let actual = f.store.commit_diff(revision).await.unwrap();
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep this file\n");
        assert_eq!(std::fs::read(path.join("HEAD")).unwrap(), head);
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
    let expected = f.store.diff(&f.first.to_string(), "main").await.unwrap();
    let actual = f
        .store
        .diff("--output=root-diff", "--output=head-diff")
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert!(!path.join("root-diff...--output=head-diff").exists());
    assert_eq!(std::fs::read(path.join("HEAD")).unwrap(), head);

    let root = f.repo.find_commit(f.first).unwrap();
    f.repo
        .tag(
            "root-release",
            root.as_object(),
            &signature(),
            "release",
            false,
        )
        .unwrap();
    let annotated = f.store.commit_diff("root-release").await.unwrap();
    let root_diff = f.store.commit_diff(&f.first.to_string()).await.unwrap();
    assert_eq!(
        serde_json::to_value(annotated).unwrap(),
        serde_json::to_value(root_diff).unwrap()
    );
    let annotated_range = f.store.diff("root-release", "main").await.unwrap();
    assert_eq!(
        serde_json::to_value(annotated_range).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[tokio::test]
async fn tree_history_and_diffs_treat_filenames_literally() {
    let f = fixture().await;
    let literal = ":(literal)notes.txt";
    let third = commit(
        &f.repo,
        "main",
        Some(f.second),
        literal,
        "literal original\n",
        "literal file",
    );
    let fourth = commit(
        &f.repo,
        "main",
        Some(third),
        "notes.txt",
        "ordinary content\n",
        "ordinary file",
    );
    let fifth = commit(
        &f.repo,
        "main",
        Some(fourth),
        literal,
        "literal changed\n",
        "change literal file",
    );
    let tree = f.store.tree("main", "").await.unwrap();
    let literal_entry = tree.iter().find(|entry| entry.name == literal).unwrap();
    let ordinary_entry = tree.iter().find(|entry| entry.name == "notes.txt").unwrap();
    assert_eq!(
        literal_entry.last_commit.as_ref().unwrap().sha,
        fifth.to_string()
    );
    assert_eq!(
        ordinary_entry.last_commit.as_ref().unwrap().sha,
        fourth.to_string()
    );
    for diff in [
        f.store.commit_diff(&fifth.to_string()).await.unwrap(),
        f.store.diff(&fourth.to_string(), "main").await.unwrap(),
    ] {
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].filename, literal);
        assert!(diff.files[0].patch.contains("-literal original\n"));
        assert!(diff.files[0].patch.contains("+literal changed\n"));
        assert!(!diff.files[0].patch.contains("ordinary content"));
    }

    let first_tree = f
        .repo
        .find_tree(with_file(&f.repo, None, literal, "literal root\n"))
        .unwrap();
    let tree = f
        .repo
        .find_tree(with_file(
            &f.repo,
            Some(&first_tree),
            "notes.txt",
            "ordinary root\n",
        ))
        .unwrap();
    let author = signature();
    let root = f
        .repo
        .commit(
            Some("refs/heads/literal-root"),
            &author,
            &author,
            "root",
            &tree,
            &[],
        )
        .unwrap();
    let diff = f.store.commit_diff(&root.to_string()).await.unwrap();
    assert_eq!(diff.files.len(), 2);
    let literal_patch = &diff
        .files
        .iter()
        .find(|file| file.filename == literal)
        .unwrap()
        .patch;
    assert!(literal_patch.contains("+literal root\n"));
    assert!(!literal_patch.contains("ordinary root"));
    let ordinary_patch = &diff
        .files
        .iter()
        .find(|file| file.filename == "notes.txt")
        .unwrap()
        .patch;
    assert!(ordinary_patch.contains("+ordinary root\n"));
    assert!(!ordinary_patch.contains("literal root"));
}

#[tokio::test]
async fn diffs_report_add_delete_modify_and_rename_paths() {
    let f = fixture().await;
    let parent = f.repo.find_commit(f.second).unwrap();
    let parent_tree = parent.tree().unwrap();
    let renamed_blob = f.repo.blob(b"rename me\n").unwrap();
    let mut base = f.repo.treebuilder(Some(&parent_tree)).unwrap();
    base.insert(
        "deleted.txt",
        f.repo.blob(b"delete me\n").unwrap(),
        0o100644,
    )
    .unwrap();
    base.insert("old.txt", renamed_blob, 0o100644).unwrap();
    base.insert("modified.txt", f.repo.blob(b"before\n").unwrap(), 0o100644)
        .unwrap();
    let base = f.repo.find_tree(base.write().unwrap()).unwrap();
    let author = signature();
    let base_commit = f
        .repo
        .commit(
            Some("refs/heads/diff-status"),
            &author,
            &author,
            "base",
            &base,
            &[&parent],
        )
        .unwrap();

    let mut head = f.repo.treebuilder(Some(&base)).unwrap();
    head.remove("deleted.txt").unwrap();
    head.remove("old.txt").unwrap();
    head.insert("new.txt", renamed_blob, 0o100644).unwrap();
    head.insert("modified.txt", f.repo.blob(b"after\n").unwrap(), 0o100644)
        .unwrap();
    head.insert("added.txt", f.repo.blob(b"added\n").unwrap(), 0o100644)
        .unwrap();
    let head = f.repo.find_tree(head.write().unwrap()).unwrap();
    let base_commit = f.repo.find_commit(base_commit).unwrap();
    let head_commit = f
        .repo
        .commit(
            Some("refs/heads/diff-status"),
            &author,
            &author,
            "head",
            &head,
            &[&base_commit],
        )
        .unwrap();

    let diff = f.store.commit_diff(&head_commit.to_string()).await.unwrap();
    let status = |path: &str| {
        diff.files
            .iter()
            .find(|file| file.filename == path)
            .map(|file| file.status.clone())
    };
    assert_eq!(diff.changed_files, 4);
    assert_eq!(status("added.txt").as_deref(), Some("added"));
    assert_eq!(status("deleted.txt").as_deref(), Some("removed"));
    assert_eq!(status("modified.txt").as_deref(), Some("modified"));
    assert_eq!(status("new.txt").as_deref(), Some("renamed"));
    assert!(!diff.files.iter().any(|file| file.filename == "old.txt"));
}

#[tokio::test]
async fn oversized_patches_return_the_typed_too_large_error() {
    let f = fixture().await;
    let content = "changed line\n".repeat(MAX_DIFF_FILE_PATCH_BYTES / 4);
    let head = commit(
        &f.repo,
        "large-diff",
        Some(f.second),
        "large-diff.txt",
        &content,
        "large diff",
    );
    assert!(f
        .store
        .commit_diff(&head.to_string())
        .await
        .unwrap_err()
        .is_response_too_large());
}

#[tokio::test]
async fn oversized_commit_and_tag_metadata_return_the_typed_too_large_error() {
    let f = fixture().await;
    let parent = f.repo.find_commit(f.second).unwrap();
    let tree = parent.tree().unwrap();
    let author = signature();
    let message = "x".repeat(MAX_COMMIT_METADATA_BYTES);
    let commit = f
        .repo
        .commit(
            Some("refs/heads/large-commit"),
            &author,
            &author,
            &message,
            &tree,
            &[&parent],
        )
        .unwrap();
    assert!(f
        .store
        .commit(&commit.to_string())
        .await
        .unwrap_err()
        .is_response_too_large());
    assert!(f
        .store
        .commits("large-commit", 0, 1, &Default::default())
        .await
        .unwrap_err()
        .is_response_too_large());

    let tag_message = "x".repeat(MAX_TAG_METADATA_BYTES);
    f.repo
        .tag(
            "large-tag",
            parent.as_object(),
            &author,
            &tag_message,
            false,
        )
        .unwrap();
    assert!(f.store.tags().await.unwrap_err().is_response_too_large());
}

#[tokio::test]
async fn tree_width_accepts_the_limit_and_rejects_one_more() {
    let f = fixture().await;
    let blob = f.repo.blob(b"entry\n").unwrap();
    let mut tree = f.repo.treebuilder(None).unwrap();
    for index in 0..MAX_TREE_ENTRIES {
        tree.insert(format!("file-{index:04}.txt"), blob, 0o100644)
            .unwrap();
    }
    let tree_id = tree.write().unwrap();
    let tree = f.repo.find_tree(tree_id).unwrap();
    let author = signature();
    let exact = f
        .repo
        .commit(
            Some("refs/heads/wide-tree"),
            &author,
            &author,
            "wide tree",
            &tree,
            &[],
        )
        .unwrap();
    assert_eq!(
        f.store.tree(&exact.to_string(), "").await.unwrap().len(),
        MAX_TREE_ENTRIES
    );

    let mut over = f.repo.treebuilder(Some(&tree)).unwrap();
    over.insert("one-too-many.txt", blob, 0o100644).unwrap();
    let over = f.repo.find_tree(over.write().unwrap()).unwrap();
    let exact = f.repo.find_commit(exact).unwrap();
    let over = f
        .repo
        .commit(
            Some("refs/heads/wide-tree"),
            &author,
            &author,
            "tree is too wide",
            &over,
            &[&exact],
        )
        .unwrap();
    assert!(f
        .store
        .tree(&over.to_string(), "")
        .await
        .unwrap_err()
        .is_response_too_large());
}

#[test]
fn a_repository_that_cannot_be_opened_is_an_outage() {
    use git2::{Error, ErrorClass, ErrorCode};
    let kind = |class| {
        RepoError::from(anyhow::Error::from(Error::new(
            ErrorCode::NotFound,
            class,
            "missing",
        )))
        .kind
    };
    assert_eq!(kind(ErrorClass::Os), ErrorKind::Unavailable);
    assert_eq!(kind(ErrorClass::Repository), ErrorKind::Unavailable);
    assert_eq!(kind(ErrorClass::Reference), ErrorKind::NotFound);
    assert_eq!(kind(ErrorClass::Tree), ErrorKind::MissingPath);
}

#[tokio::test]
async fn failures_say_what_is_missing() {
    let f = fixture().await;
    let kind = |error: RepoError| error.kind;
    assert_eq!(
        kind(f.store.tree("no-such-branch", "").await.unwrap_err()),
        ErrorKind::NotFound
    );
    assert_eq!(
        kind(f.store.tree("main", "no/such/dir").await.unwrap_err()),
        ErrorKind::MissingPath
    );
    assert_eq!(
        kind(f.store.blob("main", "nope.txt").await.unwrap_err()),
        ErrorKind::MissingPath
    );
    assert_eq!(
        kind(f.store.commit("not a revision").await.unwrap_err()),
        ErrorKind::NotFound
    );
    let nowhere = RepoStore::from_disk_path("/nonexistent/repository.git");
    assert!(nowhere.refs().await.is_err());
    assert!(!nowhere.exists().await);
    // Errors cross the wire as JSON too.
    let error = f.store.tree("main", "no/such/dir").await.unwrap_err();
    assert_eq!(round_trip(&error), error);
}

#[tokio::test]
async fn branches_and_tags_can_be_made_and_deleted() {
    let f = fixture().await;
    let authority = authority(&f).await;
    let (first, second) = (f.first.to_string(), f.second.to_string());
    assert_eq!(
        f.store
            .create_branch_as(
                &authority.pool,
                authority.repo_id,
                authority.actor,
                "feature",
                &first,
            )
            .await
            .unwrap(),
        first
    );
    assert_eq!(f.store.refs().await.unwrap()["refs/heads/feature"], first);
    assert!(f
        .store
        .create_branch_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            "feature",
            &first,
        )
        .await
        .is_err());
    f.store
        .delete_branch_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            "feature",
        )
        .await
        .unwrap();
    assert!(f
        .store
        .delete_branch_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            "feature",
        )
        .await
        .is_err());
    assert!(
        f.store
            .delete_branch_as(&authority.pool, authority.repo_id, authority.actor, "main",)
            .await
            .is_err(),
        "the default branch stays"
    );
    assert_eq!(
        f.store
            .create_branch_as(
                &authority.pool,
                authority.repo_id,
                authority.actor,
                "équipe/功能",
                &first,
            )
            .await
            .unwrap(),
        first
    );
    assert!(f
        .store
        .refs()
        .await
        .unwrap()
        .contains_key("refs/heads/équipe/功能"));
    f.store
        .delete_branch_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            "équipe/功能",
        )
        .await
        .unwrap();

    f.store
        .create_branch_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            "release/stable",
            &first,
        )
        .await
        .unwrap();
    let branch_refs = f.store.refs().await.unwrap();
    for alias in [
        "release//stable",
        "/release/stable",
        "release/stable.",
        "release/.stable",
        "release/stable.lock",
    ] {
        assert_eq!(
            f.store
                .create_branch_as(
                    &authority.pool,
                    authority.repo_id,
                    authority.actor,
                    alias,
                    &second,
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Invalid,
            "create {alias}"
        );
        assert_eq!(
            f.store
                .delete_branch_as(&authority.pool, authority.repo_id, authority.actor, alias,)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Invalid,
            "delete {alias}"
        );
        assert_eq!(f.store.refs().await.unwrap(), branch_refs, "{alias}");
    }

    let utf8_tag = f
        .store
        .create_tag_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            "版本-β",
            &second,
            None,
            "Tagger",
            "tagger@example.invalid",
        )
        .await
        .unwrap();
    assert_eq!(utf8_tag.name, "版本-β");
    f.store
        .delete_tag_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            "版本-β",
        )
        .await
        .unwrap();

    let tag = f
        .store
        .create_tag_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            "v2",
            &second,
            Some("two"),
            "Tagger",
            "tagger@example.invalid",
        )
        .await
        .unwrap();
    assert_eq!(tag.message.as_deref(), Some("two"));
    assert!(f.store.refs().await.unwrap().contains_key("refs/tags/v2"));
    let tag_refs = f.store.refs().await.unwrap();
    for alias in ["v2//candidate", "/v2", "v2.", ".v2", "v2.lock"] {
        assert_eq!(
            f.store
                .create_tag_as(
                    &authority.pool,
                    authority.repo_id,
                    authority.actor,
                    alias,
                    &second,
                    None,
                    "Tagger",
                    "tagger@example.invalid",
                )
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Invalid,
            "create {alias}"
        );
        assert_eq!(
            f.store
                .delete_tag_as(&authority.pool, authority.repo_id, authority.actor, alias,)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Invalid,
            "delete {alias}"
        );
        assert_eq!(f.store.refs().await.unwrap(), tag_refs, "{alias}");
    }
    f.store
        .delete_tag_as(&authority.pool, authority.repo_id, authority.actor, "v2")
        .await
        .unwrap();
    assert!(f
        .store
        .delete_tag_as(&authority.pool, authority.repo_id, authority.actor, "v2",)
        .await
        .is_err());
}

#[tokio::test]
async fn storage_is_created_forked_and_removed() {
    let root = tempfile::tempdir().unwrap();
    let source = RepoStore::create(root.path(), "main", None).await.unwrap();
    assert!(source.location().starts_with(root.path().to_str().unwrap()));
    assert!(source.location().ends_with(".git"));
    assert!(source.exists().await);
    let repo = git2::Repository::open_bare(source.location()).unwrap();
    commit(&repo, "main", None, "README.md", "# source\n", "first");

    let fork = RepoStore::create(root.path(), "main", Some(&source))
        .await
        .unwrap();
    assert_ne!(fork.location(), source.location());
    assert_eq!(fork.refs().await.unwrap(), source.refs().await.unwrap());

    fork.remove().await.unwrap();
    assert!(!fork.exists().await);
    fork.remove().await.unwrap();
    assert!(source.exists().await, "removing a fork leaves its source");

    let entries = std::fs::read_dir(root.path()).unwrap().count();
    assert!(RepoStore::create(root.path(), "not a branch name", None)
        .await
        .is_err());
    assert_eq!(
        std::fs::read_dir(root.path()).unwrap().count(),
        entries,
        "a failed creation leaves nothing behind"
    );
}

#[tokio::test]
async fn cross_repository_diffs_borrow_objects_without_importing() {
    let f = fixture().await;
    let fork = RepoStore::create(
        &f._root.path().join("space\"quote\ncolon:"),
        "main",
        Some(&f.store),
    )
    .await
    .unwrap();
    let fork_repo = git2::Repository::open_bare(fork.location()).unwrap();
    let base = commit(
        &f.repo,
        "main",
        Some(f.second),
        "base-only.txt",
        "base only\n",
        "base advanced",
    );
    let head = commit(
        &fork_repo,
        "main",
        Some(f.second),
        "secret.txt",
        "private content\n",
        "private head",
    );
    let base_refs = f.store.refs().await.unwrap();
    let head_refs = fork.refs().await.unwrap();
    assert_eq!(
        f.store.commit(&head.to_string()).await.unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        fork.commit(&base.to_string()).await.unwrap_err().kind,
        ErrorKind::NotFound
    );

    let diff = f.store.diff_from(&fork, "main", "main").await.unwrap();
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].filename, "secret.txt");
    assert!(diff.files[0].patch.contains("+private content\n"));
    assert!(!diff.files[0].patch.contains("base only"));
    assert_eq!(
        f.store.commit(&head.to_string()).await.unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        fork.commit(&base.to_string()).await.unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(f.store.refs().await.unwrap(), base_refs);
    assert_eq!(fork.refs().await.unwrap(), head_refs);
    assert!(!std::path::Path::new(f.store.location())
        .join("objects/info/alternates")
        .exists());
    assert!(!std::path::Path::new(fork.location())
        .join("objects/info/alternates")
        .exists());
    assert_eq!(f.store.commit("main").await.unwrap().sha, base.to_string());
    assert_eq!(fork.commit("main").await.unwrap().sha, head.to_string());
}

#[tokio::test]
async fn storage_that_cannot_be_removed_is_reported_not_swallowed() {
    let root = tempfile::tempdir().unwrap();
    // Storage that is already gone counts as removed.
    let missing = root.path().join("missing.git");
    RepoStore::from_disk_path(missing.to_str().unwrap())
        .remove()
        .await
        .unwrap();
    // A directory below a regular file cannot be removed. The caller is told, and
    // the message does not say where the directory is.
    std::fs::write(root.path().join("file"), b"not a directory").unwrap();
    let stuck = root.path().join("file").join("repo.git");
    let error = RepoStore::from_disk_path(stuck.to_str().unwrap())
        .remove()
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unavailable);
    assert!(!error.message.contains(root.path().to_str().unwrap()));
}

#[tokio::test]
async fn saving_a_file_checks_where_the_branch_was() {
    let f = fixture().await;
    let authority = authority(&f).await;
    let write = FileWrite {
        branch: "main".into(),
        path: "docs/notes.md".into(),
        message: "add notes".into(),
        expected_revision: f.second.to_string(),
        creating: true,
        deleting: false,
    };
    let saved = f
        .store
        .save_file_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            &write,
            b"notes",
            "Author",
            "author@example.invalid",
        )
        .await
        .unwrap();
    assert_eq!(saved.parents, vec![f.second.to_string()]);
    assert_eq!(
        f.store
            .blob_bytes("main", "docs/notes.md")
            .await
            .unwrap()
            .bytes,
        b"notes"
    );
    // The branch moved, so the same request is stale now and changes nothing.
    let stale = f
        .store
        .save_file_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            &write,
            b"again",
            "Author",
            "author@example.invalid",
        )
        .await
        .unwrap_err();
    assert_eq!(stale.kind, ErrorKind::Conflict);
    assert_eq!(f.store.refs().await.unwrap()["refs/heads/main"], saved.sha);
    let mut bad = write.clone();
    bad.path = "../escape".into();
    let refused = f
        .store
        .save_file_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            &bad,
            b"x",
            "Author",
            "author@example.invalid",
        )
        .await
        .unwrap_err();
    assert_eq!(refused.kind, ErrorKind::Invalid);
}

#[tokio::test]
async fn merging_moves_the_base_only_when_nothing_changed_since_approval() {
    let f = fixture().await;
    let authority = authority(&f).await;
    let head = commit(
        &f.repo,
        "feature",
        Some(f.first),
        "docs/a.md",
        "a",
        "feature",
    );
    let request = MergeRequest {
        head_branch: "feature".into(),
        base_branch: "main".into(),
        head: head.to_string(),
        base: f.second.to_string(),
        message: "Merge feature".into(),
        name: "Merger".into(),
        email: "merger@example.invalid".into(),
        method: MergeMethod::Merge,
        author_name: None,
        author_email: None,
    };
    let merged = f.store.prepare_merge(&request).await.unwrap();
    f.store
        .publish_merge_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            &request.head_branch,
            &request.base_branch,
            &request.head,
            &request.base,
            &merged,
            MergeMethod::Merge,
        )
        .await
        .unwrap();
    assert_eq!(f.store.refs().await.unwrap()["refs/heads/main"], merged);
    let parents = f.store.commit(&merged).await.unwrap().parents;
    assert_eq!(parents, vec![f.second.to_string(), head.to_string()]);
    // main moved, so the approval is stale.
    assert!(f
        .store
        .publish_merge_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            &request.head_branch,
            &request.base_branch,
            &request.head,
            &request.base,
            &merged,
            MergeMethod::Merge
        )
        .await
        .is_err());

    let left = commit(
        &f.repo,
        "left",
        Some(f.first),
        "README.md",
        "# left\n",
        "left",
    );
    let right = commit(
        &f.repo,
        "right",
        Some(f.first),
        "README.md",
        "# right\n",
        "right",
    );
    let conflicting = MergeRequest {
        head_branch: "left".into(),
        base_branch: "right".into(),
        head: left.to_string(),
        base: right.to_string(),
        message: "Merge left".into(),
        name: "Merger".into(),
        email: "merger@example.invalid".into(),
        method: MergeMethod::Merge,
        author_name: None,
        author_email: None,
    };
    assert!(f.store.prepare_merge(&conflicting).await.is_err());
    assert_eq!(
        f.store.refs().await.unwrap()["refs/heads/right"],
        right.to_string()
    );
}

/// A rebase of the head onto `base`, with the same committer as `merge_request`.
fn rebase_request(head: git2::Oid, base: git2::Oid) -> MergeRequest {
    MergeRequest {
        method: MergeMethod::Rebase,
        ..merge_request(
            &head.to_string(),
            &base.to_string(),
            "",
            "Merger",
            "merger@example.invalid",
        )
    }
}

#[tokio::test]
async fn rebase_drops_a_commit_whose_change_the_base_already_has() {
    let f = fixture().await;
    let first = commit(&f.repo, "dup", Some(f.second), "docs/a.md", "a\n", "add a");
    let second = commit(
        &f.repo,
        "dup",
        Some(first),
        "docs/a.md",
        "a\n",
        "add a again",
    );
    let tip = f
        .store
        .prepare_merge(&rebase_request(second, f.second))
        .await
        .unwrap();
    let tip = f
        .repo
        .find_commit(git2::Oid::from_str(&tip).unwrap())
        .unwrap();
    // The second commit adds nothing to the first, so only the first one is written.
    assert_eq!(tip.parent_id(0).unwrap(), f.second);
    assert_eq!(tip.summary(), Some("add a"));
}

#[tokio::test]
async fn rebase_refuses_a_conflict_and_names_the_file() {
    let f = fixture().await;
    let base = commit(
        &f.repo,
        "trunk",
        Some(f.second),
        "README.md",
        "right\n",
        "right",
    );
    let head = commit(
        &f.repo,
        "left",
        Some(f.second),
        "README.md",
        "left\n",
        "left",
    );
    let error = f
        .store
        .prepare_merge(&rebase_request(head, base))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Conflict, "{error}");
    assert!(error.message.contains("README.md"), "{error}");
}

#[tokio::test]
async fn rebase_refuses_a_history_with_a_merge_commit() {
    let f = fixture().await;
    let feature = commit(&f.repo, "feature", Some(f.second), "docs/a.md", "a\n", "a");
    let side = commit(&f.repo, "side", Some(f.second), "docs/b.md", "b\n", "b");
    let feature_commit = f.repo.find_commit(feature).unwrap();
    let side_commit = f.repo.find_commit(side).unwrap();
    let author = signature();
    let merge = f
        .repo
        .commit(
            None,
            &author,
            &author,
            "merge side",
            &feature_commit.tree().unwrap(),
            &[&feature_commit, &side_commit],
        )
        .unwrap();
    let error = f
        .store
        .prepare_merge(&rebase_request(merge, f.second))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid, "{error}");
    assert!(error.message.contains("merge commits"), "{error}");
}

#[tokio::test]
async fn rebase_replays_at_most_one_hundred_commits() {
    let f = fixture().await;
    for (branch, count) in [("hundred", 100), ("hundred-and-one", 101)] {
        let mut head = f.second;
        for index in 0..count {
            head = commit(
                &f.repo,
                branch,
                Some(head),
                &format!("files/{index}.txt"),
                "x\n",
                &format!("commit {index}"),
            );
        }
        let result = f.store.prepare_merge(&rebase_request(head, f.second)).await;
        if count == 100 {
            let tip = result.unwrap();
            assert_ne!(tip, head.to_string());
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.kind, ErrorKind::Invalid, "{error}");
            assert!(error.message.contains("at most 100"), "{error}");
        }
    }
}

#[tokio::test]
async fn rebase_keeps_each_commits_author_and_date() {
    let f = fixture().await;
    let original = git2::Signature::new(
        "Original Author",
        "original@example.invalid",
        &git2::Time::new(1_600_000_000, 120),
    )
    .unwrap();
    let parent = f.repo.find_commit(f.second).unwrap();
    let tree = f
        .repo
        .find_tree(with_file(
            &f.repo,
            Some(&parent.tree().unwrap()),
            "docs/a.md",
            "a\n",
        ))
        .unwrap();
    let head = f
        .repo
        .commit(
            Some("refs/heads/authored"),
            &original,
            &signature(),
            "Add a",
            &tree,
            &[&parent],
        )
        .unwrap();
    let tip = f
        .store
        .prepare_merge(&rebase_request(head, f.second))
        .await
        .unwrap();
    let written = f
        .repo
        .find_commit(git2::Oid::from_str(&tip).unwrap())
        .unwrap();
    let author = written.author();
    assert_eq!(author.name(), Some("Original Author"));
    assert_eq!(author.email(), Some("original@example.invalid"));
    assert_eq!(author.when().seconds(), 1_600_000_000);
    assert_eq!(author.when().offset_minutes(), 120);
    assert_eq!(written.committer().name(), Some("Merger"));
    assert_eq!(written.summary(), Some("Add a"));
}

#[tokio::test]
async fn publish_refuses_a_merge_method_turned_off_after_preparation() {
    let f = fixture().await;
    let authority = authority(&f).await;
    let head = commit(
        &f.repo,
        "feature",
        Some(f.second),
        "docs/a.md",
        "a\n",
        "add a",
    )
    .to_string();
    let base = f.second.to_string();
    let request = MergeRequest {
        method: MergeMethod::Squash,
        ..merge_request(&head, &base, "Squash a", "Merger", "merger@example.invalid")
    };
    let merged = f.store.prepare_merge(&request).await.unwrap();
    // Squash is turned off after preparation. The storage node reads the setting under its lock.
    db::repositories::records::set_merge_methods(
        &authority.pool,
        authority.actor,
        authority.repo_id,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: Some(false),
            rebase: None,
        },
    )
    .await
    .unwrap();
    let error = f
        .store
        .publish_merge_as(
            &authority.pool,
            authority.repo_id,
            authority.actor,
            "feature",
            "main",
            &head,
            &base,
            &merged,
            MergeMethod::Squash,
        )
        .await
        .unwrap_err();
    assert!(error.is_merge_method_not_allowed(), "{error}");
    assert_eq!(f.store.refs().await.unwrap()["refs/heads/main"], base);
}

#[tokio::test]
async fn squash_bounds_the_subjects_it_lists() {
    let f = fixture().await;
    // Six subjects of 200 KiB each add up to more than the output limit of one Git command.
    let subject = "s".repeat(200 * 1024);
    let mut head = f.second;
    for index in 0..6 {
        head = commit(
            &f.repo,
            "wide",
            Some(head),
            &format!("wide/{index}.txt"),
            "x\n",
            &subject,
        );
    }
    let request = MergeRequest {
        method: MergeMethod::Squash,
        ..merge_request(
            &head.to_string(),
            &f.second.to_string(),
            "Wide",
            "Merger",
            "merger@example.invalid",
        )
    };
    let squashed = f.store.prepare_merge(&request).await.unwrap();
    let written = f
        .repo
        .find_commit(git2::Oid::from_str(&squashed).unwrap())
        .unwrap();
    let message = written.message().unwrap();
    assert!(message.len() <= 8192, "{} bytes", message.len());
    assert!(message.starts_with("Wide\n\n* "), "{message}");
    assert_eq!(written.parent_id(0).unwrap(), f.second);
}

#[tokio::test]
async fn rebase_refuses_a_commit_that_would_outgrow_the_object_limit() {
    let f = fixture().await;
    // A message just under the limit, with a short committer, is a commit that Git stores and reads.
    let message = "m".repeat(crate::backend::git::MAX_COMMIT_OBJECT_BYTES - 300);
    let large = commit(
        &f.repo,
        "large",
        Some(f.second),
        "docs/a.md",
        "a\n",
        &message,
    );
    // Each replayed commit takes the merger as committer, here with the longest identity allowed.
    let name = "n".repeat(crate::backend::git::MAX_COMMIT_NAME_BYTES);
    let email = format!(
        "{}@users.noreply",
        "e".repeat(crate::backend::git::MAX_COMMIT_NAME_BYTES)
    );
    let request = MergeRequest {
        method: MergeMethod::Rebase,
        ..merge_request(&large.to_string(), &f.second.to_string(), "", &name, &email)
    };
    let error = f.store.prepare_merge(&request).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid, "{error}");
    assert!(error.message.contains("too large"), "{error}");
    assert_eq!(
        f.store.refs().await.unwrap()["refs/heads/main"],
        f.second.to_string()
    );

    // The same identity rebases a commit with a short message.
    let small = commit(
        &f.repo,
        "small",
        Some(f.second),
        "docs/b.md",
        "b\n",
        "small",
    );
    let request = MergeRequest {
        method: MergeMethod::Rebase,
        ..merge_request(&small.to_string(), &f.second.to_string(), "", &name, &email)
    };
    let tip = f.store.prepare_merge(&request).await.unwrap();
    let written = f
        .repo
        .find_commit(git2::Oid::from_str(&tip).unwrap())
        .unwrap();
    assert_eq!(written.committer().name(), Some(name.as_str()));
}

#[tokio::test]
async fn git_sessions_advertise_refs_and_serve_a_fetch() {
    let f = fixture().await;
    let advertisement = f
        .store
        .advertise_refs(GitService::UploadPack)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&advertisement).contains("refs/heads/main"));
    let want = format!("want {}\n", f.second);
    let request = format!("{:04x}{want}00000009done\n", want.len() + 4);
    let mut response = Vec::new();
    let status = f
        .store
        .git_session(
            GitSession::new(GitService::UploadPack, true),
            None,
            GitStreams {
                input: &mut request.as_bytes(),
                output: &mut response,
                errors: &mut tokio::io::sink(),
            },
        )
        .await
        .unwrap();
    assert!(status.success());
    assert!(response.windows(4).any(|window| window == b"PACK"));
}

#[tokio::test]
async fn ci_reads_pick_a_commit_and_the_workflow_files_in_it() {
    let f = fixture().await;
    let workflow =
        "name: CI\non: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: []\n";
    let third = commit(
        &f.repo,
        "main",
        Some(f.second),
        ".github/workflows/ci.yml",
        workflow,
        "add ci",
    );
    let tip = round_trip(
        &f.store
            .resolve_ci_commit("main", None, false)
            .await
            .unwrap(),
    );
    assert_eq!(
        tip,
        CiCommit {
            full_ref: "refs/heads/main".into(),
            sha: third.to_string()
        }
    );
    let older = f
        .store
        .resolve_ci_commit("main", Some(&f.first.to_string()), true)
        .await
        .unwrap();
    assert_eq!(older.sha, f.first.to_string());
    let stray = commit(&f.repo, "stray", None, "x", "x", "unrelated");
    let unrelated = f
        .store
        .resolve_ci_commit("main", Some(&stray.to_string()), true)
        .await
        .unwrap_err();
    assert_eq!(unrelated.message, "CI SHA is unrelated to requested ref");

    let sha = third.to_string();
    let file = round_trip(
        &f.store
            .ci_workflow_file(&sha, ".github/workflows/ci.yml")
            .await
            .unwrap(),
    );
    assert_eq!(file.content, workflow);
    let missing = f
        .store
        .ci_workflow_file(&sha, ".github/workflows/none.yml")
        .await
        .unwrap_err();
    assert_eq!(missing.message, "CI workflow unavailable at admitted SHA");

    let work = tempfile::tempdir().unwrap();
    let checkout = work.path().join("repo");
    f.store.checkout(&checkout, &sha).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(checkout.join(".github/workflows/ci.yml")).unwrap(),
        workflow
    );
    let cloned = git2::Repository::open(&checkout).unwrap();
    assert_eq!(cloned.head().unwrap().target().unwrap(), third);
}

#[tokio::test]
async fn commit_and_pull_request_diffs_keep_the_json_shape() {
    let f = fixture().await;
    let (first, second) = (f.first.to_string(), f.second.to_string());
    let diffs = [
        f.store.commit_diff(&second).await.unwrap(),
        f.store.diff(&first, &second).await.unwrap(),
    ];

    for diff in diffs {
        assert_eq!(diff.files.len(), 1);
        let patch = &diff.files[0].patch;
        assert!(patch.contains("+pub fn answer() -> u32 { 42 }\n"));
        let mut value = serde_json::to_value(&diff).unwrap();
        // The file's blob id is the one addition; the rest of the shape is unchanged.
        assert_eq!(value["files"][0]["blob_sha"].as_str().unwrap().len(), 40);
        value["files"][0]
            .as_object_mut()
            .unwrap()
            .remove("blob_sha");
        assert_eq!(
            value,
            serde_json::json!({
                "files": [{
                    "filename": "src/lib.rs",
                    "status": "added",
                    "additions": 1,
                    "deletions": 0,
                    "patch": patch,
                }],
                "additions": 1,
                "deletions": 0,
                "changed_files": 1,
            })
        );
    }
}

#[test]
fn unavailable_reasons_preserve_the_released_rpc_fields() {
    for (error, message) in [
        (
            RepoError::storage_node_unavailable(),
            "storage node unavailable",
        ),
        (RepoError::sessions_busy(), "Git sessions busy"),
        (
            RepoError::authority_unavailable(),
            "Repository authority is unavailable",
        ),
    ] {
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"kind": "Unavailable", "message": message})
        );
        assert_eq!(round_trip(&error), error);
    }
    let error = RepoError::from(
        anyhow::Error::new(crate::backend::git::repository::process::SessionsBusy)
            .context("private detail"),
    );
    assert_eq!(error.reason, Some(RepoErrorReason::SessionsBusy));
    assert_eq!(error.message, "Git sessions busy");
    let missing = RepoError::branch_missing();
    assert_eq!(
        serde_json::to_value(&missing).unwrap(),
        serde_json::json!({"kind": "Conflict", "message": "The branch does not exist."})
    );
    assert!(round_trip(&missing).is_branch_missing());
}

#[test]
fn repository_uuid_boundary_rejects_traversal_components() {
    for invalid in [".", "..", "../etc", "a/b", "a\\b"] {
        assert!(rpc::repository_uuid(invalid).is_err(), "{invalid}");
    }
}

async fn write_version(authority: &Authority) -> i64 {
    db::cluster::get_write_version(&authority.pool, authority.repo_id)
        .await
        .unwrap()
        .unwrap()
}

/// Each ref change on the primary adds exactly one to the write version, and a refused
/// or repeated change adds none. Replicas copy a repository when its version moves.
#[tokio::test]
async fn every_ref_change_moves_the_write_version() {
    let f = fixture().await;
    let authority = authority(&f).await;
    let (pool, repo_id, actor) = (&authority.pool, authority.repo_id, authority.actor);
    // Only a repository that a storage node holds has replicas to tell.
    let node = db::cluster::register_own_node(
        pool,
        &crate::backend::cluster::NodeKey::generate().public_hex(),
        "store-node",
        &["storage".to_owned()],
        "test",
    )
    .await
    .unwrap();
    db::cluster::bump_write_version(pool, repo_id, None)
        .await
        .unwrap();
    assert_eq!(write_version(&authority).await, 0, "no node holds it yet");
    assert!(db::cluster::place_repository(pool, repo_id, node.id)
        .await
        .unwrap());
    let (first, second) = (f.first.to_string(), f.second.to_string());
    let ahead = commit(
        &f.repo,
        "scratch",
        Some(f.second),
        "ahead.txt",
        "ahead\n",
        "ahead",
    )
    .to_string();
    assert_eq!(write_version(&authority).await, 0);

    f.store
        .create_branch_as(pool, repo_id, actor, "feature", &first)
        .await
        .unwrap();
    assert_eq!(write_version(&authority).await, 1, "create branch");
    f.store
        .delete_branch_as(pool, repo_id, actor, "feature")
        .await
        .unwrap();
    assert_eq!(write_version(&authority).await, 2, "delete branch");

    f.store
        .create_tag_as(
            pool,
            repo_id,
            actor,
            "v1",
            &first,
            None,
            "Tester",
            "tester@example.invalid",
        )
        .await
        .unwrap();
    assert_eq!(write_version(&authority).await, 3, "create tag");
    f.store
        .delete_tag_as(pool, repo_id, actor, "v1")
        .await
        .unwrap();
    assert_eq!(write_version(&authority).await, 4, "delete tag");

    let write = FileWrite {
        branch: "main".into(),
        path: "docs/notes.md".into(),
        message: "add notes".into(),
        expected_revision: second.clone(),
        creating: true,
        deleting: false,
    };
    f.store
        .save_file_as(
            pool,
            repo_id,
            actor,
            &write,
            b"notes",
            "Author",
            "author@example.invalid",
        )
        .await
        .unwrap();
    assert_eq!(write_version(&authority).await, 5, "save file");

    f.store
        .create_branch_as(pool, repo_id, actor, "topic", &ahead)
        .await
        .unwrap();
    assert_eq!(write_version(&authority).await, 6, "create topic");
    let base = f.store.refs().await.unwrap()["refs/heads/main"].clone();
    let merged = f
        .store
        .prepare_merge(&merge_request(
            &ahead,
            &base,
            "Merge topic",
            "Tester",
            "tester@example.invalid",
        ))
        .await
        .unwrap();
    f.store
        .publish_merge_as(
            pool,
            repo_id,
            actor,
            "topic",
            "main",
            &ahead,
            &base,
            &merged,
            MergeMethod::Merge,
        )
        .await
        .unwrap();
    assert_eq!(write_version(&authority).await, 7, "merge");

    f.store
        .fetch_pull_head(pool, repo_id, repo_id, actor, &f.store, 7, &ahead, None)
        .await
        .unwrap();
    assert_eq!(write_version(&authority).await, 8, "pull request head");
    f.store
        .fetch_pull_head(pool, repo_id, repo_id, actor, &f.store, 7, &ahead, None)
        .await
        .unwrap();
    assert_eq!(
        write_version(&authority).await,
        8,
        "a repeated pull request head"
    );

    assert!(f
        .store
        .create_branch_as(pool, repo_id, actor, "topic", &ahead)
        .await
        .is_err());
    assert_eq!(write_version(&authority).await, 8, "a refused branch");

    // HEAD moves with the default branch, and replicas copy HEAD, so the change is a write.
    f.store
        .set_default_branch_as(pool, repo_id, actor, "topic")
        .await
        .unwrap();
    assert_eq!(write_version(&authority).await, 9, "default branch");
    assert!(f
        .store
        .set_default_branch_as(pool, repo_id, actor, "missing")
        .await
        .is_err());
    assert_eq!(
        write_version(&authority).await,
        9,
        "a missing default branch"
    );
    // The right to administer is checked in the update's transaction, not only by the caller.
    let outsider = db::users::accounts::create_user(pool, "default_outsider", None)
        .await
        .unwrap();
    assert!(f
        .store
        .set_default_branch_as(pool, repo_id, outsider.id, "main")
        .await
        .is_err());
    // The refused attempt may count as a write, since it is counted before the update decides, but
    // the default branch stays where it was.
    let default = db::repositories::records::get_repository_by_id(pool, repo_id)
        .await
        .unwrap()
        .unwrap()
        .default_branch;
    assert_eq!(
        default, "topic",
        "a default branch change by a user who may not administer"
    );
    // An archived repository refuses the change before it counts as a write.
    let before = write_version(&authority).await;
    assert!(
        db::repositories::records::set_repository_archived(pool, repo_id, true)
            .await
            .unwrap()
    );
    assert!(f
        .store
        .set_default_branch_as(pool, repo_id, actor, "main")
        .await
        .is_err());
    assert_eq!(
        write_version(&authority).await,
        before,
        "a default branch change on an archived repository"
    );
}

/// A storage node registered in the database, to place or copy a repository on.
async fn storage_node(pool: &db::DbPool, name: &str) -> i64 {
    let key = crate::backend::cluster::NodeKey::generate().public_hex();
    let id = db::cluster::register_own_node(pool, &key, name, &["storage".to_owned()], "test")
        .await
        .unwrap()
        .id;
    // A promotion needs its target to be reporting in.
    db::cluster::record_node_heartbeat(pool, &key, "test", "{}", None)
        .await
        .unwrap();
    id
}

/// Asserts that a store call was refused because this node no longer holds the repository.
fn assert_write_denied<T>(result: Result<T, RepoError>) {
    match result {
        Ok(_) => panic!("a store for the old primary wrote after promotion"),
        Err(error) => assert!(error.is_write_denied(), "{error}"),
    }
}

/// A store opened for a storage node stops writing once the repository row names another
/// node, so a request that started on the old primary cannot publish there. A store made from
/// a bare path names no node, so it keeps following the row.
#[tokio::test]
async fn a_store_for_the_old_primary_refuses_writes_after_promotion() {
    let f = fixture().await;
    let authority = authority(&f).await;
    let (pool, repo_id, actor) = (&authority.pool, authority.repo_id, authority.actor);
    let old = storage_node(pool, "fence-old").await;
    let new = storage_node(pool, "fence-new").await;
    let placed = db::cluster::place_repository(pool, repo_id, old)
        .await
        .unwrap();
    let copied = db::cluster::insert_replica(pool, repo_id, new, old)
        .await
        .unwrap();
    assert!(placed && copied);

    let (first, second) = (f.first.to_string(), f.second.to_string());
    let ahead = commit(
        &f.repo,
        "scratch",
        Some(f.second),
        "ahead.txt",
        "ahead\n",
        "ahead",
    )
    .to_string();
    let writer = RepoStore::on_node(f.store.location(), old);
    writer
        .create_branch_as(pool, repo_id, actor, "before", &first)
        .await
        .unwrap();
    writer
        .create_branch_as(pool, repo_id, actor, "topic", &ahead)
        .await
        .unwrap();
    // Only a copy that has been made can take over, even when forced.
    assert!(db::cluster::set_applied_version(pool, repo_id, new, old, 0)
        .await
        .unwrap());

    let outcome = db::cluster::promote_repository(pool, repo_id, old, new, false)
        .await
        .unwrap();
    assert!(
        matches!(outcome, db::cluster::PromoteOutcome::Promoted { .. }),
        "{outcome:?}"
    );

    let base = f.store.refs().await.unwrap()["refs/heads/main"].clone();
    let merged = f
        .store
        .prepare_merge(&merge_request(
            &ahead,
            &base,
            "Merge topic",
            "Tester",
            "tester@example.invalid",
        ))
        .await
        .unwrap();
    let write = FileWrite {
        branch: "main".into(),
        path: "docs/notes.md".into(),
        message: "add notes".into(),
        expected_revision: second.clone(),
        creating: true,
        deleting: false,
    };

    let result = writer
        .create_branch_as(pool, repo_id, actor, "after", &first)
        .await;
    assert_write_denied(result);
    let result = writer
        .save_file_as(
            pool,
            repo_id,
            actor,
            &write,
            b"notes",
            "Author",
            "author@example.invalid",
        )
        .await;
    assert_write_denied(result);
    let result = writer
        .publish_merge_as(
            pool,
            repo_id,
            actor,
            "topic",
            "main",
            &ahead,
            &base,
            &merged,
            MergeMethod::Merge,
        )
        .await;
    assert_write_denied(result);
    assert!(f.repo.find_reference("refs/heads/after").is_err());

    // A store made from a bare path names no node, so it follows the row to the new primary.
    let plain = RepoStore::from_disk_path(f.store.location());
    plain
        .create_branch_as(pool, repo_id, actor, "plain", &first)
        .await
        .unwrap();
    assert!(f.repo.find_reference("refs/heads/plain").is_ok());
}

/// A ref change that is counted but not yet made keeps the primary from handing a copy to a
/// promotion that would lose the change, and from listing its refs for a replica copy.
#[tokio::test]
async fn a_counted_ref_change_blocks_promotion_and_the_replica_listing() {
    let f = fixture().await;
    let authority = authority(&f).await;
    let pool = &authority.pool;
    let repo_id = authority.repo_id;
    let old = storage_node(pool, "fence-listing-old").await;
    let new = storage_node(pool, "fence-listing-new").await;
    assert!(db::cluster::place_repository(pool, repo_id, old)
        .await
        .unwrap());
    assert!(db::cluster::insert_replica(pool, repo_id, new, old)
        .await
        .unwrap());
    assert!(db::cluster::set_applied_version(pool, repo_id, new, old, 0)
        .await
        .unwrap());
    let writer = RepoStore::on_node(f.store.location(), old);

    let change = begin_ref_change(
        pool,
        repo_id,
        std::path::Path::new(f.store.location()),
        Some(old),
    )
    .await
    .unwrap();
    let busy = writer.replica_listing().await.unwrap_err();
    assert!(busy.is_replica_busy(), "{busy}");
    let outcome = db::cluster::promote_repository(pool, repo_id, old, new, true)
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            db::cluster::PromoteOutcome::Stale {
                applied_version: 0,
                write_version: 1
            }
        ),
        "{outcome:?}"
    );

    drop(change);
    assert!(writer.replica_listing().await.is_ok());
}

/// A change the database refuses, because another node holds the repository, is not counted,
/// so the primary that refused it keeps listing its refs for copies.
#[tokio::test]
async fn a_refused_ref_change_is_not_counted() {
    let f = fixture().await;
    let authority = authority(&f).await;
    let pool = &authority.pool;
    let repo_id = authority.repo_id;
    let holder = storage_node(pool, "refused-holder").await;
    let other = storage_node(pool, "refused-other").await;
    assert!(db::cluster::place_repository(pool, repo_id, holder)
        .await
        .unwrap());

    let refused = begin_ref_change(
        pool,
        repo_id,
        std::path::Path::new(f.store.location()),
        Some(other),
    )
    .await;
    assert!(matches!(&refused, Err(error) if error.is_write_denied()));
    assert_eq!(write_version(&authority).await, 0, "nothing was counted");
    assert!(RepoStore::on_node(f.store.location(), holder)
        .replica_listing()
        .await
        .is_ok());
}

/// An annotation larger than the tag readers accept would make every tag listing fail, so it
/// is refused when the tag is created.
#[tokio::test]
async fn an_annotated_tag_larger_than_readers_accept_is_refused() {
    use crate::backend::git::MAX_TAG_MESSAGE_BYTES;
    let f = fixture().await;
    let authority = authority(&f).await;
    let (pool, repo_id, actor) = (&authority.pool, authority.repo_id, authority.actor);
    let first = f.first.to_string();
    let message = "x".repeat(MAX_TAG_MESSAGE_BYTES + 1);
    let error = f
        .store
        .create_tag_as(
            pool,
            repo_id,
            actor,
            "big",
            &first,
            Some(&message),
            "Tester",
            "tester@example.invalid",
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert!(f.repo.find_reference("refs/tags/big").is_err());

    let message = "x".repeat(MAX_TAG_MESSAGE_BYTES);
    f.store
        .create_tag_as(
            pool,
            repo_id,
            actor,
            "largest",
            &first,
            Some(&message),
            "Tester",
            "tester@example.invalid",
        )
        .await
        .unwrap();
    let tags = f.store.tags().await.unwrap();
    assert!(tags.iter().any(|tag| tag.name == "largest"));
}

/// Pages and replicas also list a bounded number of bytes of names, so a few long names
/// count for more than a short one.
#[test]
fn the_ref_limit_counts_the_bytes_of_names() {
    use crate::backend::git::{RefsInUse, MAX_REPOSITORY_REF_BYTES};
    let held = |bytes| RefsInUse { count: 3, bytes };
    let name = "refs/heads/topic";
    let cost = crate::backend::git::ref_listing_bytes(name);
    assert!(ensure_ref_room(held(MAX_REPOSITORY_REF_BYTES - cost), &[name]).is_ok());
    let error = ensure_ref_room(held(MAX_REPOSITORY_REF_BYTES - cost + 1), &[name]).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert!(error.message.contains("total at most"), "{}", error.message);
}

/// At `MAX_REPOSITORY_REFS` branches and tags a store refuses a new one, and deleting one
/// makes room for exactly one more.
#[tokio::test]
async fn a_repository_at_the_ref_limit_refuses_new_branches_and_tags() {
    const FULL: &str = "A repository holds at most 10000 branches and tags";
    let f = fixture().await;
    let authority = authority(&f).await;
    let (pool, repo_id, actor) = (&authority.pool, authority.repo_id, authority.actor);
    let first = f.first.to_string();
    // `main` is one of the branches that count.
    for index in 1..MAX_REPOSITORY_REFS {
        let name = format!("refs/heads/topic-{index}");
        f.repo.reference(&name, f.first, false, "fill").unwrap();
    }
    let path = std::path::Path::new(f.store.location());
    let held = crate::backend::git::branches_and_tags_in_use(path).unwrap();
    assert_eq!(held.count, MAX_REPOSITORY_REFS);

    let error = f
        .store
        .create_branch_as(pool, repo_id, actor, "over", &first)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert_eq!(error.message, FULL);
    let error = f
        .store
        .create_tag_as(
            pool,
            repo_id,
            actor,
            "over",
            &first,
            None,
            "Tester",
            "tester@example.invalid",
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert_eq!(error.message, FULL);
    // Saving a file to a branch that does not exist creates it, so it is refused too.
    let write = FileWrite {
        branch: "fresh".into(),
        path: "notes.md".into(),
        message: "add notes".into(),
        expected_revision: "unborn".into(),
        creating: true,
        deleting: false,
    };
    let error = f
        .store
        .save_file_as(
            pool,
            repo_id,
            actor,
            &write,
            b"notes",
            "Author",
            "author@example.invalid",
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert_eq!(error.message, FULL);
    assert!(f.repo.find_reference("refs/heads/fresh").is_err());

    f.store
        .delete_branch_as(pool, repo_id, actor, "topic-1")
        .await
        .unwrap();
    f.store
        .create_branch_as(pool, repo_id, actor, "over", &first)
        .await
        .unwrap();
}

/// Pull request heads take the mirror limit that branches and tags leave. At that limit a new
/// head is refused, while moving an existing head still works.
#[tokio::test]
async fn the_pull_head_limit_refuses_a_new_head_and_still_moves_one() {
    const LIMIT: usize = crate::backend::git::MAX_MIRROR_REFS - MAX_REPOSITORY_REFS;
    let f = fixture().await;
    let authority = authority(&f).await;
    let (pool, repo_id, actor) = (&authority.pool, authority.repo_id, authority.actor);
    let (first, second) = (f.first.to_string(), f.second.to_string());
    // A new head is admitted below the limit.
    f.store
        .fetch_pull_head(pool, repo_id, repo_id, actor, &f.store, 1, &first, None)
        .await
        .unwrap();
    // Written directly, so the test does not publish every head through the store.
    for number in 2..=LIMIT {
        let name = format!("refs/pull/{number}/head");
        f.repo.reference(&name, f.first, false, "fill").unwrap();
    }
    let path = std::path::Path::new(f.store.location());
    // Packed, so Git reads one file instead of tens of thousands of loose refs and the
    // fetch below stays well inside its timeout on a busy machine.
    let packed = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(path)
        .args(["pack-refs", "--all"])
        .status()
        .unwrap();
    assert!(packed.success());
    let held = crate::backend::git::pull_heads_in_use(path).unwrap();
    assert_eq!(held.count, LIMIT);

    let next = i32::try_from(LIMIT + 1).unwrap();
    let error = f
        .store
        .fetch_pull_head(pool, repo_id, repo_id, actor, &f.store, next, &first, None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert_eq!(
        error.message,
        format!("A repository holds at most {LIMIT} pull request heads")
    );
    assert!(f
        .repo
        .find_reference(&format!("refs/pull/{next}/head"))
        .is_err());

    f.store
        .fetch_pull_head(
            pool,
            repo_id,
            repo_id,
            actor,
            &f.store,
            1,
            &second,
            Some(&first),
        )
        .await
        .unwrap();
    assert_eq!(f.repo.refname_to_id("refs/pull/1/head").unwrap(), f.second);
}

/// Admission is checked without a lock, so a race can leave a repository a few refs past its
/// limit. Its refs are still listed, a new branch is refused, and a branch can still be
/// deleted, except the default branch.
#[tokio::test]
async fn a_repository_past_the_ref_limit_stays_readable_and_repairable() {
    const FULL: &str = "A repository holds at most 10000 branches and tags";
    const OVERSHOOT: usize = 16;
    let f = fixture().await;
    let authority = authority(&f).await;
    let (pool, repo_id, actor) = (&authority.pool, authority.repo_id, authority.actor);
    let first = f.first.to_string();
    // Written directly, the way concurrent pushes can, so admission never counts them.
    for index in 1..MAX_REPOSITORY_REFS + OVERSHOOT {
        let name = format!("refs/heads/topic-{index}");
        f.repo.reference(&name, f.first, false, "fill").unwrap();
    }
    let total = MAX_REPOSITORY_REFS + OVERSHOOT;
    assert_eq!(f.store.refs().await.unwrap().len(), total);
    assert_eq!(f.store.info().await.unwrap().branches.len(), total);

    let error = f
        .store
        .create_branch_as(pool, repo_id, actor, "over", &first)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert_eq!(error.message, FULL);

    // The API path checks the default branch before it deletes, so it runs here too.
    use crate::backend::ci::CiRuns;
    use crate::backend::notifications::NotificationBroadcaster;
    use crate::backend::repositories::refs::{delete_branch, RefError};
    use std::sync::Arc;
    let ci = CiRuns::new(
        pool.clone(),
        Default::default(),
        Arc::new(tokio::sync::Notify::new()),
        None,
        Arc::new(NotificationBroadcaster::new(pool)),
    );
    let (owner, repo) = ("store_actor", "store-tests");
    let refused = delete_branch(pool, None, &ci, actor, owner, repo, "main").await;
    assert!(matches!(
        refused,
        Err(RefError::Invalid("Cannot delete the default branch"))
    ));
    delete_branch(pool, None, &ci, actor, owner, repo, "topic-1")
        .await
        .unwrap();
    assert_eq!(f.store.refs().await.unwrap().len(), total - 1);
}

//! Repository README previews, tree revisions, commit history and diffs over HTTP on
//! disposable SQLite and PostgreSQL data.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as http;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use codeyard::{
    backend::{
        api::create_router,
        app::AppState,
        ci::CiConfig,
        db,
        repositories::{
            lifecycle::provision,
            store::{repo_store, FileWrite, RepoStore},
        },
        storage::ObjectStore,
    },
    webui::{
        app::{Page, PageEnvelope},
        pages::repos::{self as view, tree::MAX_README_BYTES},
    },
};
use database::Database;
use std::{io::Write, sync::Arc};
use tower::ServiceExt;

#[expect(
    clippy::too_many_arguments,
    reason = "The fixture keeps publication authority explicit."
)]
async fn save(
    pool: &db::DbPool,
    repo_id: i64,
    actor: i64,
    store: &RepoStore,
    tip: &str,
    path: &str,
    bytes: &[u8],
    creating: bool,
) -> String {
    store
        .save_file_as(
            pool,
            repo_id,
            actor,
            &FileWrite {
                branch: "main".into(),
                path: path.into(),
                expected_revision: tip.into(),
                message: "README fixture".into(),
                creating,
                deleting: false,
            },
            bytes,
            "README owner",
            "readme@example.invalid",
        )
        .await
        .unwrap()
        .sha
}

async fn model(client: &http::Client, path: &str) -> (view::tree::TreePage, usize) {
    let response = client
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("Accept", "application/vnd.codeyard.page+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/vnd.codeyard.page+json"
    );
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let envelope: PageEnvelope = serde_json::from_slice(&bytes).unwrap();
    let Page::Repositories(view::page::Page::Tree { page, .. }) = envelope.page else {
        panic!("expected repository tree model");
    };
    (page, bytes.len())
}

async fn contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let actor = f.actor("readme_owner").await;
    let root = tempfile::tempdir().unwrap();
    let repository = provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "preview",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let store = repo_store(&repository, None);
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let client = http::Client::anonymous(&router);
    let tree_path = "/readme_owner/preview";
    let file_path = "/readme_owner/preview/blob/main/ReadMe.md";
    let raw_path = "/readme_owner/preview/raw/main/ReadMe.md";
    let small =
        "# Hello\r\n**bold 雪**\n<script>unsafe-marker</script>\n[link](https://example.invalid)";
    let tip = save(
        f.pool(),
        repository.id,
        actor,
        &store,
        "unborn",
        "ReadMe.md",
        small.as_bytes(),
        true,
    )
    .await;
    let (page, length) = model(&client, tree_path).await;
    assert_eq!(page.readme.as_deref(), Some(small));
    assert!(length < 32 * 1024);
    assert_eq!(page.latest_commit.unwrap().sha, tip);
    let (status, html) = client.page(tree_path).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("<strong class=\"font-semibold text-foreground\">bold 雪</strong>"));
    assert!(html.contains("&lt;script&gt;unsafe-marker&lt;/script&gt;"));
    assert!(!html.contains("<script>unsafe-marker</script>"));
    let small_page_len = html.len();
    // The file page shows the Markdown rendered by default; `?view=code` shows its source.
    // The page's stylesheet names every class, so the checks look for the class attribute. A
    // root element also gets a hydration id after its attributes, so the tag is not closed here.
    let (status, html) = client.page(file_path).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("class=\"repo-readme-body\""));
    assert!(!html.contains("Copy code"));
    let (status, html) = client.page(&format!("{file_path}?view=code")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Copy code"));
    assert!(!html.contains("class=\"repo-readme-body\""));

    // The ceiling remains renderable, including the formerly quadratic input.
    let ceiling = "[".repeat(MAX_README_BYTES as usize);
    let previous = tip.clone();
    let tip = save(
        f.pool(),
        repository.id,
        actor,
        &store,
        &tip,
        "ReadMe.md",
        ceiling.as_bytes(),
        false,
    )
    .await;
    let (page, _) = model(&client, tree_path).await;
    assert_eq!(page.readme.as_deref(), Some(ceiling.as_str()));
    let (status, html) = client.page(tree_path).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(&ceiling));
    assert!(!html.contains("not rendered inline"));

    // A file that only the branch has. While a same-named tag points at the older commit,
    // the homepage, the file, raw and archive must all show the branch.
    let tip = save(
        f.pool(),
        repository.id,
        actor,
        &store,
        &tip,
        "notes.txt",
        b"branch notes",
        true,
    )
    .await;
    // Reads select the branch when a same-named tag has older content.
    let bare = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let tagged = bare
        .find_object(git2::Oid::from_str(&previous).unwrap(), None)
        .unwrap();
    for annotated in [false, true] {
        if annotated {
            let signature = git2::Signature::now("Tagger", "tagger@example.invalid").unwrap();
            bare.tag("main", &tagged, &signature, "older README", false)
                .unwrap();
        } else {
            bare.tag_lightweight("main", &tagged, false).unwrap();
        }
        let (page, _) = model(&client, tree_path).await;
        assert_eq!(page.reference, "main");
        assert_eq!(page.latest_commit.unwrap().sha, tip);
        assert_eq!(page.readme.as_deref(), Some(ceiling.as_str()));
        let (status, html) = client.page(tree_path).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(&ceiling));
        let (status, html) = client
            .page("/readme_owner/preview/blob/main/notes.txt")
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("branch notes"));
        let (status, body) = client
            .page("/readme_owner/preview/raw/main/notes.txt")
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "branch notes");
        let (status, body) = client.page("/readme_owner/preview/archive?ref=main").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("preview-main/notes.txt"));
        bare.tag_delete("main").unwrap();
    }

    // A branch literally named `refs/heads/main` is stored as `refs/heads/refs/heads/main`.
    // The full name must still resolve to the real branch, which is what CI runs.
    let spoof = git2::Oid::from_str(&previous).unwrap();
    bare.reference("refs/heads/refs/heads/main", spoof, false, "spoof branch")
        .unwrap();
    let ci = store
        .resolve_ci_commit("refs/heads/main", None, false)
        .await
        .unwrap();
    assert_eq!(ci.full_ref, "refs/heads/main");
    assert_eq!(ci.sha, tip);
    assert_eq!(store.commit("refs/heads/main").await.unwrap().sha, tip);
    bare.find_reference("refs/heads/refs/heads/main")
        .unwrap()
        .delete()
        .unwrap();

    // A nested branch can exist in pushed data. Its short name no longer resolves and CI
    // refuses it, but its full name still reads it.
    let topic = git2::Oid::from_str(&previous).unwrap();
    bare.reference(
        "refs/heads/refs/heads/topic",
        topic,
        false,
        "nested topic branch",
    )
    .unwrap();
    assert!(store.commit("refs/heads/topic").await.is_err());
    assert!(store
        .resolve_ci_commit("refs/heads/topic", None, false)
        .await
        .is_err());
    assert_eq!(
        store
            .commit("refs/heads/refs/heads/topic")
            .await
            .unwrap()
            .sha,
        previous
    );
    bare.find_reference("refs/heads/refs/heads/topic")
        .unwrap()
        .delete()
        .unwrap();

    let oversized = format!("{ceiling}oversized-readme-marker");
    let tip = save(
        f.pool(),
        repository.id,
        actor,
        &store,
        &tip,
        "ReadMe.md",
        oversized.as_bytes(),
        false,
    )
    .await;
    let (page, length) = model(&client, tree_path).await;
    assert!(page.readme.is_none());
    assert_eq!(
        page.entries
            .iter()
            .find(|entry| entry.name == "ReadMe.md")
            .unwrap()
            .size,
        Some(oversized.len() as u64)
    );
    assert!(
        length < 32 * 1024,
        "oversized README must not enter the navigation model"
    );
    let (status, html) = client.page(tree_path).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("This README is larger than 500 KiB and is not rendered inline."));
    assert!(html.contains(&format!("href=\"{file_path}\"")));
    assert!(!html.contains("oversized-readme-marker"));
    // The page includes the inline repository stylesheet, so bound it by the page that showed
    // the small README rather than a fixed size. A README rendered inline would still exceed this.
    assert!(
        html.len() < small_page_len + 8 * 1024,
        "{} bytes, small README page {} bytes",
        html.len(),
        small_page_len
    );
    let (status, file) = client.page(file_path).await;
    assert_eq!(status, StatusCode::OK);
    assert!(file.contains(&format!("href=\"{raw_path}\"")));
    let raw = client.exchange("GET", raw_path, "", Vec::new()).await;
    assert_eq!(raw.status, StatusCode::OK);
    assert_eq!(raw.body, oversized.as_bytes());
    assert_eq!(store.commit("main").await.unwrap().sha, tip);

    let directory = "# Directory\n*directory preview*\n";
    let tip = save(
        f.pool(),
        repository.id,
        actor,
        &store,
        &tip,
        "docs/README.md",
        directory.as_bytes(),
        true,
    )
    .await;
    let (page, _) = model(&client, "/readme_owner/preview/tree/main/docs").await;
    assert_eq!(page.readme.as_deref(), Some(directory));
    let (status, html) = client.page("/readme_owner/preview/tree/main/docs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("<em class=\"italic\">directory preview</em>"));

    // A small source can expand past the rendering budget solely through its directory.
    let deep_directory = vec!["d".repeat(255); 12].join("/");
    let deep_readme = format!("{deep_directory}/README.md");
    assert_eq!(deep_readme.len(), 3_081);
    let deep_tree = format!("/readme_owner/preview/tree/main/{deep_directory}");
    let mut tip = tip;
    for (index, (line, count)) in [
        ("[x](x)\n", 4_000),
        ("![x](x)\n", 4_000),
        ("[x](x)\n", 70_000),
        ("[x](x) ![x](x)\n", 1),
    ]
    .into_iter()
    .enumerate()
    {
        let source = format!("<script>budget-marker</script>\n{}", line.repeat(count));
        assert!(source.len() <= MAX_README_BYTES as usize);
        tip = save(
            f.pool(),
            repository.id,
            actor,
            &store,
            &tip,
            &deep_readme,
            source.as_bytes(),
            index == 0,
        )
        .await;
        let (page, length) = model(&client, &deep_tree).await;
        assert_eq!(page.readme.as_deref(), Some(source.as_str()));
        assert!(length < 1024 * 1024);
        let (status, html) = client.page(&deep_tree).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.len() < 2 * 1024 * 1024);
        assert!(html.contains("&lt;script&gt;budget-marker&lt;/script&gt;"));
        assert!(!html.contains("<script>budget-marker</script>"));
        let href = format!("href=\"/readme_owner/preview/blob/main/{deep_directory}/x\"");
        let src = format!("src=\"/readme_owner/preview/raw/main/{deep_directory}/x\"");
        if count == 1 {
            assert!(!html.contains("showing a plain-text preview"));
            assert!(html.contains(&href));
            assert!(html.contains(&src));
        } else {
            assert!(html.contains("showing a plain-text preview"));
            assert!(html.contains(line.trim()));
            assert!(!html.contains(&href));
            assert!(!html.contains(&src));
        }
        let raw_path = format!("/readme_owner/preview/raw/main/{deep_readme}");
        let raw = client.exchange("GET", &raw_path, "", Vec::new()).await;
        assert_eq!(raw.status, StatusCode::OK);
        assert_eq!(raw.body, source.as_bytes());
    }

    let binary = b"\0binary-readme-marker";
    let tip = save(
        f.pool(),
        repository.id,
        actor,
        &store,
        &tip,
        "ReadMe.md",
        binary,
        false,
    )
    .await;
    let (page, _) = model(&client, tree_path).await;
    assert!(page.readme.is_none());
    let (status, html) = client.page(tree_path).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!html.contains("id=\"readme\""));
    assert!(!html.contains("binary-readme-marker"));
    let raw = client.exchange("GET", raw_path, "", Vec::new()).await;
    assert_eq!(raw.status, StatusCode::OK);
    assert_eq!(raw.body, binary);
    assert_eq!(store.commit("main").await.unwrap().sha, tip);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_readme_previews_are_bounded() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_readme_previews_are_bounded() {
    contract(true).await;
}

/// The last commit page a reader can ask for has no Next link, and the page before it
/// has one. Page 1,000 starts 29,970 commits in, so the history is written with git
/// fast-import instead of through the editor.
async fn commit_history_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let actor = f.actor("history_owner").await;
    let root = tempfile::tempdir().unwrap();
    let repository = provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "history",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let store = repo_store(&repository, None);
    let tip = save(
        f.pool(),
        repository.id,
        actor,
        &store,
        "unborn",
        "LICENSE",
        b"history\n",
        true,
    )
    .await;
    // The tree page takes its latest commit from a listing sorted by commit time, so
    // the imported commits must be newer than the first commit, as real history is.
    let root_time = store.commit(&tip).await.unwrap().committer_timestamp;
    let mut stream = String::new();
    let mut parent = format!("from {tip}");
    for mark in 1..=30_000 {
        let message = if mark <= 5_000 {
            "x".repeat(1024)
        } else {
            "commit".into()
        };
        let content = mark.to_string();
        stream.push_str(&format!(
            "commit refs/heads/main\nmark :{mark}\ncommitter History <history@example.invalid> {} +0000\ndata {}\n{message}\n{parent}\nM 100644 inline src/code\ndata {}\n{content}\n\n",
            root_time + mark, message.len(), content.len()
        ));
        parent = format!("from :{mark}");
        if mark == 5_000 {
            stream.push_str("reset refs/heads/output-budget\nfrom :5000\n\n");
        }
    }
    // These histories hit the field and response-metadata budgets without making
    // the directory itself too wide or any commit object oversized.
    for (branch, message, paths) in [
        (
            "field-budget",
            "x".repeat(4097),
            vec!["src/code".to_owned()],
        ),
        (
            "metadata-budget",
            "x".repeat(1024),
            (0..1000).map(|index| format!("file-{index:04}")).collect(),
        ),
    ] {
        stream.push_str(&format!(
            "commit refs/heads/{branch}\ncommitter History <history@example.invalid> {} +0000\ndata {}\n{message}\nfrom :30000\n",
            root_time + 30_001,
            message.len()
        ));
        if branch == "metadata-budget" {
            stream.push_str("deleteall\n");
        }
        for path in paths {
            stream.push_str(&format!("M 100644 inline {path}\ndata 7\nchanged\n"));
        }
        stream.push('\n');
    }
    let mut git = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repository.disk_path.as_str())
        .args(["fast-import", "--quiet"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    git.stdin
        .take()
        .unwrap()
        .write_all(stream.as_bytes())
        .unwrap();
    assert!(git.wait().unwrap().success());
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let client = http::Client::anonymous(&router);
    let head = store.commit("main").await.unwrap().sha;
    let commits = client
        .expect(
            "GET",
            "/api/repos/history_owner/history/commits?ref=main&per_page=1",
            serde_json::Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(commits.as_array().unwrap().len(), 1);
    assert_eq!(commits[0]["sha"], head);
    let (status, html) = client
        .page("/history_owner/history/commits?ref=main&page=999")
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("page=1000"), "{html}");
    let (status, html) = client
        .page("/history_owner/history/commits?ref=main&page=1000")
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(!html.contains("page=1001"), "{html}");
    let (status, html) = client
        .page("/history_owner/history/commits?ref=main&page=1001")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{html}");

    let tip = store.commit("main").await.unwrap().sha;
    for (branch, count) in [
        ("main", 2),
        ("output-budget", 2),
        ("field-budget", 2),
        ("metadata-budget", 1000),
    ] {
        let response = client
            .exchange(
                "GET",
                &format!("/api/repos/history_owner/history/tree/{branch}"),
                "",
                Vec::new(),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text());
        let entries: Vec<serde_json::Value> = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(entries.len(), count);
        for entry in &entries {
            let expected = (branch == "main" && entry["name"] == "src").then_some(tip.as_str());
            assert_eq!(entry["last_commit"]["sha"].as_str(), expected);
            if expected.is_none() {
                assert!(entry["last_commit"].is_null());
            }
        }
        if count == 2 {
            assert_eq!(entries[0]["name"], "src");
            assert_eq!(entries[0]["type"], "dir");
            assert_eq!(entries[1]["name"], "LICENSE");
            assert_eq!(entries[1]["size"], 8);
        }
        let path = format!("/history_owner/history/tree/{branch}");
        let (page, _) = model(&client, &path).await;
        assert_eq!(
            page.entries.len(),
            count,
            "{branch}: entries {:?}, latest {:?}, tip {tip}",
            page.entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            page.latest_commit
                .as_ref()
                .map(|commit| commit.sha.as_str()),
        );
        for entry in &page.entries {
            let expected = (branch == "main" && entry.name == "src").then_some(tip.as_str());
            assert_eq!(
                entry.last_commit.as_ref().map(|commit| commit.sha.as_str()),
                expected
            );
        }
        let (status, html) = client.page(&path).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(html.contains(if count == 2 { "LICENSE" } else { "file-0999" }));
    }
    f.finish().await;
}

async fn browse_fixture(
    pg: bool,
) -> (
    Database,
    tempfile::TempDir,
    db::models::Repository,
    http::Client,
) {
    bundle::install();
    let f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let actor = f.actor("browse_owner").await;
    let root = tempfile::tempdir().unwrap();
    let repository = provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "browse",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let client = http::Client::login(&router, f.pool(), actor).await;
    (f, root, repository, client)
}

fn browse_commit(
    repo: &git2::Repository,
    parent: Option<git2::Oid>,
    timestamp: i64,
    message: &str,
    files: &[(&str, &[u8], i32)],
) -> git2::Oid {
    let mut tree = repo.treebuilder(None).unwrap();
    for (path, bytes, mode) in files {
        tree.insert(path, repo.blob(bytes).unwrap(), *mode).unwrap();
    }
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    let signature = git2::Signature::new(
        "Browse",
        "browse@example.invalid",
        &git2::Time::new(timestamp, 0),
    )
    .unwrap();
    let parent = parent.map(|oid| repo.find_commit(oid).unwrap());
    repo.commit(
        None,
        &signature,
        &signature,
        message,
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

async fn revision_and_chrome_contract(pg: bool) {
    let (mut f, _root, repository, client) = browse_fixture(pg).await;
    let repo = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let parent = browse_commit(
        &repo,
        None,
        1_800_000_000,
        "newer parent",
        &[
            ("README.md", b"parent snapshot\n", 0o100644),
            ("parent-only", b"parent\n", 0o100644),
        ],
    );
    let tip = browse_commit(
        &repo,
        Some(parent),
        1_700_000_000,
        "older child",
        &[
            ("README.md", b"child snapshot\n", 0o100644),
            ("child-only", b"child\n", 0o100644),
        ],
    );
    repo.reference("refs/heads/main", tip, true, "fixture")
        .unwrap();
    for revision in ["main".to_owned(), tip.to_string()] {
        let (page, _) = model(&client, &format!("/browse_owner/browse/tree/{revision}")).await;
        assert_eq!(page.latest_commit.unwrap().sha, tip.to_string());
        assert_eq!(page.readme.as_deref(), Some("child snapshot\n"));
        assert!(page.entries.iter().any(|entry| entry.name == "child-only"));
        assert!(!page.entries.iter().any(|entry| entry.name == "parent-only"));
        let raw = client
            .exchange(
                "GET",
                &format!("/browse_owner/browse/raw/{revision}/README.md"),
                "",
                Vec::new(),
            )
            .await;
        assert_eq!(raw.status, StatusCode::OK);
        assert_eq!(raw.body, b"child snapshot\n");
    }

    // An unrelated broken history makes --count --all fail, but must not affect
    // repository chrome or raw reads from a healthy branch.
    let tree = repo.find_commit(tip).unwrap().tree_id();
    let broken = repo.odb().unwrap().write(git2::ObjectType::Commit, format!(
        "tree {tree}\nparent {}\nauthor Browse <browse@example.invalid> 1700000000 +0000\ncommitter Browse <browse@example.invalid> 1700000000 +0000\n\nbroken ancestor\n", "1".repeat(40),
    ).as_bytes()).unwrap();
    repo.reference("refs/heads/broken", broken, true, "fixture")
        .unwrap();
    assert!(repo_store(&repository, None).commit_count().await.is_err());
    let (page, _) = model(&client, "/browse_owner/browse").await;
    assert_eq!(page.latest_commit.unwrap().sha, tip.to_string());
    let raw_path = "/browse_owner/browse/raw/main/README.md";
    let raw = client.exchange("GET", raw_path, "", Vec::new()).await;
    assert_eq!(raw.status, StatusCode::OK);
    assert_eq!(raw.body, b"child snapshot\n");
    let response = client
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(raw_path)
                .header("Range", "bytes=0-4")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
        b"child"
    );
    f.finish().await;
}

async fn bounded_ancestors_contract(pg: bool) {
    let (mut f, _root, repository, client) = browse_fixture(pg).await;
    let repo = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let files = [("README.md", b"snapshot\n".as_slice(), 0o100644)];
    let oversized = browse_commit(
        &repo,
        None,
        1_800_000_000,
        &"x".repeat(codeyard::backend::repositories::store::MAX_COMMIT_METADATA_BYTES + 1),
        &files,
    );
    let tip = browse_commit(&repo, Some(oversized), 1_700_000_000, "small tip", &files);
    repo.reference("refs/heads/main", tip, true, "fixture")
        .unwrap();
    let commits = client
        .expect(
            "GET",
            "/api/repos/browse_owner/browse/commits?ref=main&per_page=1",
            serde_json::Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(commits.as_array().unwrap().len(), 1);
    assert_eq!(commits[0]["sha"], tip.to_string());
    for query in ["per_page=2", "per_page=1&page=2", "per_page=1&page=3"] {
        client
            .expect(
                "GET",
                &format!("/api/repos/browse_owner/browse/commits?ref=main&{query}"),
                serde_json::Value::Null,
                StatusCode::PAYLOAD_TOO_LARGE,
            )
            .await;
    }

    // Individually acceptable skipped objects still consume a finite work budget.
    let message = "x".repeat(250 * 1024);
    let mut parent = None;
    for index in 0..270 {
        parent = Some(browse_commit(
            &repo,
            parent,
            1_700_000_000 + index,
            &message,
            &files,
        ));
    }
    let tip = browse_commit(&repo, parent, 1_800_000_000, "budget tip", &files);
    repo.reference("refs/heads/main", tip, true, "fixture")
        .unwrap();
    client
        .expect(
            "GET",
            "/api/repos/browse_owner/browse/commits?ref=main&per_page=1&page=1000",
            serde_json::Value::Null,
            StatusCode::PAYLOAD_TOO_LARGE,
        )
        .await;
    let commits = client
        .expect(
            "GET",
            "/api/repos/browse_owner/browse/commits?ref=main&per_page=1",
            serde_json::Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(commits[0]["sha"], tip.to_string());
    f.finish().await;
}

async fn type_change_contract(pg: bool) {
    let (mut f, _root, repository, client) = browse_fixture(pg).await;
    let repo = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let mut parent = browse_commit(
        &repo,
        None,
        1_700_000_000,
        "regular files",
        &[
            ("a-change", b"regular\n", 0o100644),
            ("z-other", b"before\n", 0o100644),
        ],
    );
    repo.reference("refs/heads/main", parent, true, "fixture")
        .unwrap();
    for (index, mode) in [0o120000, 0o100644].into_iter().enumerate() {
        // Git counts no lines for a type change whose contents are unchanged, so the
        // reverse case uses different text to keep the line counts meaningful.
        let content: &[u8] = if mode == 0o120000 {
            b"target\n"
        } else {
            b"regular\n"
        };
        let tip = browse_commit(
            &repo,
            Some(parent),
            1_700_000_001 + index as i64,
            "type change",
            &[
                ("a-change", content, mode),
                ("z-other", format!("after {index}\n").as_bytes(), 0o100644),
            ],
        );
        let base = format!("base-{index}");
        let head = format!("topic-{index}");
        repo.reference(&format!("refs/heads/{base}"), parent, true, "fixture")
            .unwrap();
        repo.reference(&format!("refs/heads/{head}"), tip, true, "fixture")
            .unwrap();
        let commit = client
            .expect(
                "GET",
                &format!("/api/repos/browse_owner/browse/commits/{tip}"),
                serde_json::Value::Null,
                StatusCode::OK,
            )
            .await;
        let pull = client.expect("POST", "/api/repos/browse_owner/browse/pulls", serde_json::json!({
            "title": format!("Type change {index}"), "head_branch": head, "base_branch": base,
        }), StatusCode::CREATED).await;
        let diff = client
            .expect(
                "GET",
                &format!(
                    "/api/repos/browse_owner/browse/pulls/{}/diff",
                    pull["number"]
                ),
                serde_json::Value::Null,
                StatusCode::OK,
            )
            .await;
        for diff in [&commit["diff"], &diff] {
            assert_eq!(diff["changed_files"], 2);
            let files = diff["files"].as_array().unwrap();
            assert_eq!(files[0]["filename"], "a-change");
            assert_eq!(files[0]["status"], "type_changed");
            assert_eq!(
                files[0]["patch"]
                    .as_str()
                    .unwrap()
                    .matches("diff --git ")
                    .count(),
                2
            );
            assert_eq!(files[1]["filename"], "z-other");
            assert!(files[1]["patch"]
                .as_str()
                .unwrap()
                .contains(&format!("+after {index}")));
            assert_eq!(diff["additions"], 2);
            assert_eq!(diff["deletions"], 2);
        }
        for path in [
            format!("/browse_owner/browse/commit/{tip}"),
            format!("/browse_owner/browse/pulls/new?head={head}&base={base}&intent=compare"),
        ] {
            let (status, html) = client.page(&path).await;
            assert_eq!(status, StatusCode::OK, "{html}");
            assert!(html.contains("a-change"));
            assert!(html.contains("z-other"));
        }
        parent = tip;
    }
    f.finish().await;
}

/// A folder's header shows the newest commit that touched the folder. The root keeps the
/// branch head, which here changed a file outside the folder.
async fn folder_header_contract() {
    bundle::install();
    let f = Database::new(false).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let actor = f.actor("folder_owner").await;
    let root = tempfile::tempdir().unwrap();
    let repository = provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "folders",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let store = repo_store(&repository, None);
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let client = http::Client::anonymous(&router);
    // Commit times have one-second resolution, so each commit waits for the clock to move on.
    let docs_first = save(
        f.pool(),
        repository.id,
        actor,
        &store,
        "unborn",
        "docs/a.md",
        b"a\n",
        true,
    )
    .await;
    let readme = save(
        f.pool(),
        repository.id,
        actor,
        &store,
        &docs_first,
        "README.md",
        b"# Hi\n",
        true,
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let docs_second = save(
        f.pool(),
        repository.id,
        actor,
        &store,
        &readme,
        "docs/b.md",
        b"b\n",
        true,
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let head = save(
        f.pool(),
        repository.id,
        actor,
        &store,
        &docs_second,
        "README.md",
        b"# Hi again\n",
        false,
    )
    .await;
    let (page, _) = model(&client, "/folder_owner/folders/tree/main/docs").await;
    assert_eq!(page.latest_commit.unwrap().sha, docs_second);
    let (page, _) = model(&client, "/folder_owner/folders/tree/main").await;
    assert_eq!(page.latest_commit.unwrap().sha, head);
}

#[tokio::test]
async fn sqlite_folder_header_shows_the_folder_last_commit() {
    folder_header_contract().await;
}

#[tokio::test]
async fn sqlite_tree_revision_and_lightweight_chrome() {
    revision_and_chrome_contract(false).await;
}
#[tokio::test]
async fn postgres_tree_revision_and_lightweight_chrome() {
    revision_and_chrome_contract(true).await;
}
#[tokio::test]
async fn sqlite_commit_ancestors_are_bounded() {
    bounded_ancestors_contract(false).await;
}
#[tokio::test]
async fn postgres_commit_ancestors_are_bounded() {
    bounded_ancestors_contract(true).await;
}
#[tokio::test]
async fn sqlite_type_changes_keep_complete_diffs() {
    type_change_contract(false).await;
}
#[tokio::test]
async fn postgres_type_changes_keep_complete_diffs() {
    type_change_contract(true).await;
}

async fn merge_tree_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let actor = f.actor("merge_owner").await;
    let root = tempfile::tempdir().unwrap();
    let repository = provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "resolution",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let bare = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let commit = |message: &str, files: &[(&str, &str)], parents: &[git2::Oid], time: i64| {
        let mut docs = bare.treebuilder(None).unwrap();
        for (name, content) in files {
            docs.insert(name, bare.blob(content.as_bytes()).unwrap(), 0o100644)
                .unwrap();
        }
        let mut tree = bare.treebuilder(None).unwrap();
        tree.insert("docs", docs.write().unwrap(), 0o040000)
            .unwrap();
        if parents.len() == 2 {
            let empty = bare.treebuilder(None).unwrap().write().unwrap();
            tree.insert("empty", empty, 0o040000).unwrap();
        }
        let tree = bare.find_tree(tree.write().unwrap()).unwrap();
        let parents: Vec<_> = parents
            .iter()
            .map(|id| bare.find_commit(*id).unwrap())
            .collect();
        let signature = git2::Signature::new(
            "Merger",
            "merger@example.invalid",
            &git2::Time::new(time, 0),
        )
        .unwrap();
        bare.commit(
            None,
            &signature,
            &signature,
            message,
            &tree,
            &parents.iter().collect::<Vec<_>>(),
        )
        .unwrap()
    };
    let base = commit("base", &[("README.md", "base")], &[], 1_700_000_000);
    let left = commit(
        "left",
        &[("README.md", "base"), ("left.txt", "left")],
        &[base],
        1_700_000_001,
    );
    let right = commit(
        "right",
        &[("README.md", "base"), ("right.txt", "right")],
        &[base],
        1_700_000_002,
    );
    let merge = commit(
        "merge resolution",
        &[
            ("README.md", "resolved"),
            ("left.txt", "left"),
            ("right.txt", "right"),
            ("resolution.txt", "merge only"),
        ],
        &[left, right],
        1_700_000_003,
    );
    bare.reference("refs/heads/main", merge, false, "fixture")
        .unwrap();
    bare.set_head("refs/heads/main").unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let client = http::Client::anonymous(&router);
    let entries = client
        .expect(
            "GET",
            "/api/repos/merge_owner/resolution/tree/main/docs",
            serde_json::Value::Null,
            StatusCode::OK,
        )
        .await;
    for (name, sha) in [
        ("README.md", merge),
        ("resolution.txt", merge),
        ("left.txt", left),
        ("right.txt", right),
    ] {
        let entry = entries
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap();
        assert_eq!(entry["last_commit"]["sha"], sha.to_string(), "{name}");
    }
    let (page, _) = model(&client, "/merge_owner/resolution/tree/main/docs").await;
    assert_eq!(page.readme.as_deref(), Some("resolved"));
    assert_eq!(page.entries.len(), 4);
    let (page, _) = model(&client, "/merge_owner/resolution").await;
    let empty = page
        .entries
        .iter()
        .find(|entry| entry.name == "empty")
        .unwrap();
    assert!(empty.last_commit.is_none());
    let (status, html) = client.page("/merge_owner/resolution/tree/main/docs").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("resolution.txt"));
    f.finish().await;
}

#[tokio::test]
async fn sqlite_merge_resolution_tree_history() {
    merge_tree_contract(false).await;
}
#[tokio::test]
async fn postgres_merge_resolution_tree_history() {
    merge_tree_contract(true).await;
}

#[tokio::test]
async fn sqlite_commit_history_stops_at_the_last_permitted_page() {
    commit_history_contract(false).await;
}
#[tokio::test]
async fn postgres_commit_history_stops_at_the_last_permitted_page() {
    commit_history_contract(true).await;
}

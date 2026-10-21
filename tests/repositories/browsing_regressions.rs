//! Namespace-aware browsing and database-only repository pages on both engines.
use crate::support::{database::Database, fake_webui_bundle as bundle, http_client::Client};
use axum::http::StatusCode;
use codeyard::{
    backend::{db, repositories::lifecycle},
    webui::{app::Page, pages::repos as view},
};
use serde_json::{json, Value};

fn commit(
    repo: &git2::Repository,
    reference: &str,
    parent: Option<git2::Oid>,
    text: &str,
) -> git2::Oid {
    let mut tree = repo.treebuilder(None).unwrap();
    tree.insert("file.txt", repo.blob(text.as_bytes()).unwrap(), 0o100644)
        .unwrap();
    tree.insert(
        "README.md",
        repo.blob(b"[file](file.txt)\n").unwrap(),
        0o100644,
    )
    .unwrap();
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Current", "current@example.test").unwrap();
    let parent = parent.map(|id| repo.find_commit(id).unwrap());
    repo.commit(
        Some(reference),
        &signature,
        &signature,
        text,
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

async fn contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("alice").await;
    let stranger = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repository = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), owner).await;
    let bob = Client::login(&router, f.pool(), stranger).await;
    // A first commit on a new branch has no existing source to compare against.
    let reply = alice
        .form_reply(
            "/alice/proj/_forms/write-file",
            &[
                ("repository_id", &repository.id.to_string()),
                ("branch", "main"),
                ("path", "first.txt"),
                ("intent", "save"),
                ("message", "First file"),
                ("content", "first content"),
                ("creating", "yes"),
                ("expected_revision", "unborn"),
                ("commit_target", "new_branch"),
                ("new_branch", "first-save"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert_eq!(
        reply.location.as_deref(),
        Some("/alice/proj/blob/refs/heads/first-save/first.txt")
    );
    assert_eq!(
        alice.page(reply.location.as_deref().unwrap()).await.0,
        StatusCode::OK
    );
    let git = git2::Repository::open_bare(&repository.disk_path).unwrap();
    git.find_reference("refs/heads/first-save")
        .unwrap()
        .delete()
        .unwrap();
    let first = commit(&git, "refs/heads/main", None, "main content");
    let second = commit(&git, "refs/heads/refs/topic", Some(first), "nested content");
    git.reference("refs/tags/v1", first, false, "").unwrap();
    for branch in ["refs/topic", "refs/tags/v1", "refs/heads/main"] {
        let reference = format!("refs/heads/{branch}");
        git.reference(&reference, second, true, "").unwrap();
        let (_, page) = alice.model(&format!("/alice/proj/tree/{reference}")).await;
        let Page::Repositories(view::page::Page::Tree { page, .. }) = page else {
            panic!("tree")
        };
        assert_eq!(page.reference, reference);
        assert_eq!(page.latest_commit.unwrap().sha, second.to_string());
        let (_, page) = alice
            .model(&format!("/alice/proj/blob/{reference}/file.txt"))
            .await;
        let Page::Repositories(view::page::Page::Blob { page, .. }) = page else {
            panic!("blob")
        };
        assert_eq!(page.content, "nested content");
        assert!(page.editable);
        let reply = alice
            .exchange(
                "GET",
                &format!("/alice/proj/raw/{reference}/file.txt"),
                "",
                vec![],
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.text(), "nested content");
        let (_, page) = alice
            .model(&format!("/alice/proj/edit/{reference}/file.txt"))
            .await;
        let Page::Repositories(view::page::Page::Editor { editor, .. }) = page else {
            panic!("editor")
        };
        assert_eq!(editor.branch, branch);
        assert_eq!(editor.expected_revision, second.to_string());
        let (_, page) = alice
            .model(&format!("/alice/proj/upload/{reference}/docs"))
            .await;
        let Page::Repositories(view::page::Page::Upload { form, .. }) = page else {
            panic!("upload")
        };
        assert_eq!(form.branch, branch);
    }

    // Contributors come from Git, so new commits show up immediately.
    let contributors = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/contributors",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(contributors.as_array().unwrap().len(), 1);
    assert_eq!(contributors[0]["name"], "Current");
    assert_eq!(contributors[0]["commits"], 2);
    commit(&git, "refs/heads/main", Some(first), "new main content");
    let contributors = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/contributors",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(contributors[0]["commits"], 3);

    // Query-form comparison links retain the requested view at their canonical URL.
    let reply = alice
        .exchange(
            "GET",
            "/alice/proj/compare?base=main&head=refs%2Ftopic&view=split",
            "",
            vec![],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(reply.location.as_deref().unwrap().ends_with("?view=split"));
    let (_, page) = alice.model(reply.location.as_deref().unwrap()).await;
    let Page::Collaboration(page) = page else {
        panic!("compare")
    };
    let view::issues::Page::Compare { split, .. } = *page else {
        panic!("compare")
    };
    assert!(split);

    // Branch pages need one small summary per head, not every open pull's body.
    let mut numbers = Vec::new();
    for base in ["main", "refs/heads/main"] {
        let pull = db::pull_requests::records::create_pull_request(
            f.pool(),
            db::pull_requests::records::NewPullRequest {
                repo_id: repository.id,
                head_repo_id: Some(repository.id),
                author_id: owner,
                title: base,
                body: &"x".repeat(64 * 1024),
                head_branch: "refs/topic",
                head_sha: &second.to_string(),
                base_branch: base,
                base_sha: &first.to_string(),
                is_draft: false,
            },
        )
        .await
        .unwrap();
        numbers.push(pull.number);
    }
    let summaries = db::pull_requests::records::list_open_pull_requests_from_branches(
        f.pool(),
        repository.id,
        None,
    )
    .await
    .unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].number, numbers[0]);
    let summaries = db::pull_requests::records::list_open_pull_requests_from_branches(
        f.pool(),
        repository.id,
        Some("refs/heads/main"),
    )
    .await
    .unwrap();
    assert_eq!(summaries[0].number, numbers[1]);
    let (_, page) = alice.model("/alice/proj/branches").await;
    let Page::Repositories(view::page::Page::Refs { rows, .. }) = page else {
        panic!("branches")
    };
    assert_eq!(
        rows.iter()
            .find(|row| row.name == "refs/topic")
            .unwrap()
            .pull
            .as_ref()
            .unwrap()
            .number,
        numbers[0]
    );

    git.set_head("refs/heads/refs/topic").unwrap();
    let current = f.repository(repository.id).await;
    db::repository_stats::upsert_repo_languages(
        f.pool(),
        &db::repository_stats::language_source(f.pool(), repository.id)
            .await
            .unwrap()
            .unwrap(),
        "obsolete-revision",
        &[],
    )
    .await
    .unwrap();
    let (_, page) = alice.model("/alice/proj").await;
    let Page::Repositories(view::page::Page::Tree { page, .. }) = page else {
        panic!("tree")
    };
    assert_eq!(page.reference, "refs/heads/refs/topic");
    assert_eq!(
        db::repository_stats::language_revision(f.pool(), repository.id, &current.default_branch)
            .await
            .unwrap(),
        Some(second.to_string())
    );
    git.set_head("refs/heads/unborn").unwrap();

    // Tag-only defaults keep their namespace in file and Markdown URLs.
    for branch in git.branches(Some(git2::BranchType::Local)).unwrap() {
        branch.unwrap().0.delete().unwrap();
    }
    let (_, page) = alice.model("/alice/proj").await;
    let Page::Repositories(view::page::Page::Tree { page, .. }) = page else {
        panic!("tree")
    };
    assert_eq!(page.reference, "refs/tags/v1");
    let file = view::ref_path::file_url("/alice/proj", "blob", &page.reference, "file.txt");
    assert_eq!(alice.page(&file).await.0, StatusCode::OK);
    let (_, html) = alice.page("/alice/proj").await;
    assert!(html.contains("/alice/proj/blob/refs/tags/v1/file.txt"));

    // Unreadable archived repositories are indistinguishable from missing repositories.
    f.execute(
        "UPDATE repositories SET is_private=TRUE, is_archived=TRUE WHERE id=$1",
        &[repository.id],
    )
    .await;
    bob.expect(
        "POST",
        "/api/repos/alice/proj/about",
        json!({"topics":[]}),
        StatusCode::NOT_FOUND,
    )
    .await;
    bob.expect(
        "POST",
        "/api/repos/alice/missing/about",
        json!({"topics":[]}),
        StatusCode::NOT_FOUND,
    )
    .await;
    f.execute(
        "UPDATE repositories SET is_private=FALSE, is_archived=FALSE WHERE id=$1",
        &[repository.id],
    )
    .await;

    // Database-only release pages do not enumerate a legacy oversized ref listing.
    let release = db::releases::create_release(
        f.pool(),
        owner,
        repository.id,
        "v1",
        &first.to_string(),
        &db::releases::ReleaseFields {
            name: Some("Version 1".into()),
            body: Some("Notes".into()),
            is_draft: false,
            is_prerelease: false,
            make_latest: true,
        },
    )
    .await
    .unwrap();
    for index in 0..1300 {
        git.reference(
            &format!(
                "refs/tags/{}/{}/{}/{index:04}-{}",
                "x".repeat(225),
                "y".repeat(225),
                "z".repeat(225),
                "w".repeat(225)
            ),
            first,
            false,
            "",
        )
        .unwrap();
    }
    for path in ["/alice/proj/releases", "/alice/proj/releases/tag/v1"] {
        assert_eq!(alice.page(path).await.0, StatusCode::OK, "{path}");
    }
    assert!(release.id > 0);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_browsing_regressions() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_browsing_regressions() {
    contract(true).await;
}

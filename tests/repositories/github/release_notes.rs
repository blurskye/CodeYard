use super::*;

fn note_commit(git: &git2::Repository, text: &str) -> String {
    let parent = git.head().unwrap().peel_to_commit().unwrap();
    let tree = parent.tree().unwrap();
    let mut builder = git.treebuilder(Some(&tree)).unwrap();
    builder
        .insert("README.md", git.blob(text.as_bytes()).unwrap(), 0o100644)
        .unwrap();
    let tree = git.find_tree(builder.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
    git.commit(
        Some("HEAD"),
        &signature,
        &signature,
        text,
        &tree,
        &[&parent],
    )
    .unwrap()
    .to_string()
}
async fn release_notes_contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let reader = credential(&router, &f, alice, "read").await;
    let outsider = credential(&router, &f, bob, "write").await;
    let (_, created) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"notes","auto_init":true,"private":true}),
        StatusCode::CREATED,
    )
    .await;
    let repo = f.repository(created["id"].as_i64().unwrap()).await;
    let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
    let base = git.head().unwrap().peel_to_commit().unwrap();
    let base_sha = base.id().to_string();
    git.tag_lightweight("v1", base.as_object(), false).unwrap();
    drop(base);
    for (index, title) in ["Important change", "Excluded change"]
        .into_iter()
        .enumerate()
    {
        let sha = note_commit(&git, title);
        let branch = format!("feature-{index}");
        let pull = db::pull_requests::records::create_pull_request(
            f.pool(),
            db::pull_requests::records::NewPullRequest {
                repo_id: repo.id,
                head_repo_id: None,
                author_id: alice,
                issue: None,
                maintainer_can_modify: false,
                title,
                body: "",
                head_branch: &branch,
                head_sha: &sha,
                base_branch: "main",
                base_sha: &base_sha,
                is_draft: false,
            },
        )
        .await
        .unwrap();
        assert!(db::pull_requests::records::mark_pull_requests_merged(
            f.pool(),
            std::slice::from_ref(&pull),
            alice,
            &sha
        )
        .await
        .unwrap());
        let label = db::labels::create_label(
            f.pool(),
            alice,
            repo.id,
            if index == 0 { "feature" } else { "skip-notes" },
            "#abcdef",
            None,
        )
        .await
        .unwrap();
        f.execute(
            "INSERT INTO pr_labels (pull_request_id,label_id) VALUES ($1,$2)",
            &[pull.id, label.id],
        )
        .await;
    }
    {
        let parent = git.head().unwrap().peel_to_commit().unwrap();
        let tree = parent.tree().unwrap();
        let mut settings = git.treebuilder(None).unwrap();
        for(name,text) in [("release.yml","changelog:\n  exclude:\n    labels: [skip-notes]\n  categories:\n    - title: Highlights\n      labels: [feature]\n    - title: Other\n      labels: ['*']\n"),("custom.yml","changelog:\n  categories:\n    - title: Custom\n      labels: ['*']\n"),("bad.yml","changelog: [not-a-mapping]")] {settings.insert(name,git.blob(text.as_bytes()).unwrap(),0o100644).unwrap();}
        let mut builder = git.treebuilder(Some(&tree)).unwrap();
        builder
            .insert(".github", settings.write().unwrap(), 0o040000)
            .unwrap();
        let tree = git.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
        git.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Release configuration",
            &tree,
            &[&parent],
        )
        .unwrap();
    }
    let endpoint = "/api/v3/repos/alice/notes/releases/generate-notes";
    let (_, notes) = checked(
        &router,
        "repos/generate-release-notes",
        "POST",
        endpoint,
        Some(&writer),
        json!({"tag_name":"v2"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(notes["name"], "v2");
    let body = notes["body"].as_str().unwrap();
    assert!(body.contains("## Highlights"));
    assert!(body.contains("Important change by @alice"));
    assert!(!body.contains("Excluded change"));
    assert!(body.contains("/pulls/1"));
    assert!(body.contains("/compare/v1...v2"));
    assert!(git.find_reference("refs/tags/v2").is_err());
    checked(
        &router,
        "repos/generate-release-notes",
        "POST",
        endpoint,
        Some(&reader),
        json!({"tag_name":"v2"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/generate-release-notes",
        "POST",
        endpoint,
        Some(&outsider),
        json!({"tag_name":"v2"}),
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "repos/generate-release-notes",
        "POST",
        endpoint,
        Some(&writer),
        json!({"tag_name":"v2","previous_tag_name":"missing"}),
        StatusCode::NOT_FOUND,
    )
    .await;
    for path in ["../escape.yml", ".github/bad.yml", ".github/missing.yml"] {
        checked(
            &router,
            "repos/generate-release-notes",
            "POST",
            endpoint,
            Some(&writer),
            json!({"tag_name":"v2","configuration_file_path":path}),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let(_,custom)=checked(&router,"repos/generate-release-notes","POST",endpoint,Some(&writer),json!({"tag_name":"v2","previous_tag_name":"v1","configuration_file_path":".github/custom.yml"}),StatusCode::OK).await;
    assert!(custom["body"].as_str().unwrap().contains("## Custom"));
    assert!(custom["body"].as_str().unwrap().contains("Excluded change"));
    let native = Client::login(&router, f.pool(), alice).await;
    let generated = native
        .form_reply(
            "/alice/notes/releases/new",
            &[
                ("repository_id", &repo.id.to_string()),
                ("tag", "v2"),
                ("target", "main"),
                ("body", "Native introduction"),
                ("previous_tag", "v1"),
                ("intent", "generate-notes"),
            ],
        )
        .await;
    assert_eq!(generated.status, StatusCode::OK, "{}", generated.text());
    assert!(generated.text().contains("Native introduction"));
    assert!(generated.text().contains("Highlights"));
    let (_, rows) = checked(
        &router,
        "repos/list-releases",
        "GET",
        "/api/v3/repos/alice/notes/releases",
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(rows.as_array().unwrap().is_empty());
    let (_, release) = checked(
        &router,
        "repos/create-release",
        "POST",
        "/api/v3/repos/alice/notes/releases",
        Some(&writer),
        json!({"tag_name":"v2","generate_release_notes":true,"body":"API introduction"}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(release["name"], "v2");
    assert!(release["body"]
        .as_str()
        .unwrap()
        .starts_with("API introduction\n\n## Highlights"));
    checked(
        &router,
        "repos/generate-release-notes",
        "POST",
        endpoint,
        Some(&writer),
        json!({"tag_name":"v2","target_commitish":"ignored-because-tag-exists"}),
        StatusCode::OK,
    )
    .await;
}
#[tokio::test]
async fn sqlite_github_release_notes() {
    release_notes_contract(false).await;
}
#[tokio::test]
async fn postgres_github_release_notes() {
    release_notes_contract(true).await;
}

use super::*;

async fn community_contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let (_, health) = checked(&router,"repos/create-for-authenticated-user","POST","/api/v3/user/repos",Some(&writer),json!({"name":"health","description":"Project","homepage":"https://docs.example.test","auto_init":true,"license_template":"mit"}),StatusCode::CREATED).await;
    {
        // A template directory holding only config.yml offers no templates and earns no credit.
        let health = f.repository(health["id"].as_i64().unwrap()).await;
        let git = git2::Repository::open_bare(&health.disk_path).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
        let head = git.head().unwrap().peel_to_commit().unwrap();
        let mut templates = git.treebuilder(None).unwrap();
        let config = git.blob(b"blank_issues_enabled: false\n").unwrap();
        templates.insert("config.yml", config, 0o100644).unwrap();
        let templates = templates.write().unwrap();
        let mut github = git.treebuilder(None).unwrap();
        github
            .insert("ISSUE_TEMPLATE", templates, 0o040000)
            .unwrap();
        let github = github.write().unwrap();
        let mut root = git.treebuilder(Some(&head.tree().unwrap())).unwrap();
        root.insert(".github", github, 0o040000).unwrap();
        let tree = git.find_tree(root.write().unwrap()).unwrap();
        git.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Template settings",
            &tree,
            &[&head],
        )
        .unwrap();
    }
    let endpoint = "/api/v3/repos/alice/health/community/profile";
    let (_, profile) = checked(
        &router,
        "repos/get-community-profile-metrics",
        "GET",
        endpoint,
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(profile["health_percentage"], 42);
    assert_eq!(profile["files"]["license"]["spdx_id"], "MIT");
    assert_eq!(profile["documentation"], "https://docs.example.test/");
    assert!(profile["files"]["readme"]["html_url"]
        .as_str()
        .unwrap()
        .contains("/alice/health/blob/"));
    assert!(profile["files"]["contributing"].is_null());
    let (_, created) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":".github"}),
        StatusCode::CREATED,
    )
    .await;
    let defaults = f.repository(created["id"].as_i64().unwrap()).await;
    {
        let git = git2::Repository::open_bare(&defaults.disk_path).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
        let mut builder = git.treebuilder(None).unwrap();
        for (path, content) in [
            ("CONTRIBUTING.md", "Contributing guide"),
            (
                "CODE_OF_CONDUCT.md",
                codeyard_git_storage::repositories::templates::CONTRIBUTOR_COVENANT_20,
            ),
            ("ISSUE_TEMPLATE.md", "Describe the issue"),
            ("PULL_REQUEST_TEMPLATE.md", "Describe the change"),
        ] {
            builder
                .insert(path, git.blob(content.as_bytes()).unwrap(), 0o100644)
                .unwrap();
        }
        let tree = git.find_tree(builder.write().unwrap()).unwrap();
        git.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Default community files",
            &tree,
            &[],
        )
        .unwrap();
    }
    let (_, profile) = checked(
        &router,
        "repos/get-community-profile-metrics",
        "GET",
        endpoint,
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(profile["health_percentage"], 100);
    assert_eq!(
        profile["files"]["code_of_conduct"]["key"],
        "contributor_covenant"
    );
    assert!(profile["files"]["contributing"]["html_url"]
        .as_str()
        .unwrap()
        .contains("/alice/.github/blob/"));
    let html = Client::anonymous(&router)
        .page_ok("/alice/health/insights/community")
        .await;
    assert!(html.contains("Community standards"));
    assert!(html.contains("100%"));
    assert!(html.contains("View file"));
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/alice/.github",
        Some(&writer),
        json!({"private":true}),
        StatusCode::OK,
    )
    .await;
    let (_, profile) = checked(
        &router,
        "repos/get-community-profile-metrics",
        "GET",
        endpoint,
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(profile["health_percentage"], 42);
    assert!(profile["files"]["code_of_conduct_file"].is_null());
    checked(
        &router,
        "repos/get-community-profile-metrics",
        "GET",
        "/api/v3/repos/alice/.github/community/profile",
        None,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
}
#[tokio::test]
async fn sqlite_github_community_profile() {
    community_contract(false).await;
}
#[tokio::test]
async fn postgres_github_community_profile() {
    community_contract(true).await;
}

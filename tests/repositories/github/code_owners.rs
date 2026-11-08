use super::*;

async fn diagnostics_contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let reader = credential(&router, &f, bob, "read").await;
    let (_, created) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"owners","private":true,"auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let repo = f.repository(created["id"].as_i64().unwrap()).await;
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        codeyard_database::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let endpoint = "/api/v3/repos/alice/owners/codeowners/errors";
    checked(
        &router,
        "repos/codeowners-errors",
        "GET",
        endpoint,
        Some(&reader),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
        let parent = git.head().unwrap().peel_to_commit().unwrap();
        let base = parent.tree().unwrap();
        let mut tree = git.treebuilder(Some(&base)).unwrap();
        tree.insert("CODEOWNERS", git.blob(b"* @alice\n").unwrap(), 0o100644)
            .unwrap();
        let tree = git.find_tree(tree.write().unwrap()).unwrap();
        let good = git
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Valid root file",
                &tree,
                &[&parent],
            )
            .unwrap();
        git.reference("refs/heads/good", good, false, "Fixture")
            .unwrap();
        let parent = git.find_commit(good).unwrap();
        let base = parent.tree().unwrap();
        let mut tree = git.treebuilder(Some(&base)).unwrap();
        let mut directory = git.treebuilder(None).unwrap();
        directory.insert("CODEOWNERS",git.blob(b"# Comment\n/ @alice\nsrc/ @bob\n!bad @alice\nsrc/[ab] @alice\n*.rs unknown @alice\n docs/ @missing\n").unwrap(),0o100644).unwrap();
        tree.insert(".github", directory.write().unwrap(), 0o040000)
            .unwrap();
        let tree = git.find_tree(tree.write().unwrap()).unwrap();
        git.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Invalid preferred file",
            &tree,
            &[&parent],
        )
        .unwrap();
    }
    let (_, result) = checked(
        &router,
        "repos/codeowners-errors",
        "GET",
        endpoint,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let errors = result["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 6);
    assert_eq!(errors[0]["line"], 2);
    assert_eq!(errors[0]["path"], ".github/CODEOWNERS");
    assert!(errors[0]["suggestion"]
        .as_str()
        .unwrap()
        .contains("did you mean"));
    assert_eq!(errors[1]["kind"], "Invalid owner");
    assert_eq!(errors[1]["line"], 3);
    assert_eq!(errors[4]["column"], 6);
    assert!(errors[4]["message"].as_str().unwrap().contains('^'));
    let (_, valid) = checked(
        &router,
        "repos/codeowners-errors",
        "GET",
        &format!("{endpoint}?ref=good"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(valid["errors"].as_array().unwrap().is_empty());
    checked(
        &router,
        "repos/codeowners-errors",
        "GET",
        &format!("{endpoint}?ref=missing"),
        Some(&reader),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "repos/codeowners-errors",
        "GET",
        endpoint,
        None,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let native = Client::login(&router, f.pool(), bob).await;
    let page = native
        .page_ok("/alice/owners/blob/main/.github/CODEOWNERS")
        .await;
    assert!(page.contains("CODEOWNERS diagnostics"));
    assert!(page.contains("Invalid owner on line 3"));
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        codeyard_database::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let (_, result) = checked(
        &router,
        "repos/codeowners-errors",
        "GET",
        endpoint,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(result["errors"].as_array().unwrap().len(), 5);
}
#[tokio::test]
async fn sqlite_github_codeowners_diagnostics() {
    diagnostics_contract(false).await;
}
#[tokio::test]
async fn postgres_github_codeowners_diagnostics() {
    diagnostics_contract(true).await;
}

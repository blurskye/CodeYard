use super::*;

async fn copies(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let eve = f.actor("eve").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let bob_writer = credential(&router, &f, bob, "write").await;
    let bob_reader = credential(&router, &f, bob, "read").await;
    let outsider = credential(&router, &f, eve, "write").await;
    let (_, created) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"template","private":true,"auto_init":true,"is_template":true}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(created["is_template"], true);
    let source = f.repository(created["id"].as_i64().unwrap()).await;
    let old = {
        let git = git2::Repository::open_bare(&source.disk_path).unwrap();
        let parent = git.head().unwrap().peel_to_commit().unwrap();
        let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
        let mut tree = git.treebuilder(Some(&parent.tree().unwrap())).unwrap();
        tree.insert(
            "removed-secret.txt",
            git.blob(b"not part of the template").unwrap(),
            0o100644,
        )
        .unwrap();
        let tree = git.find_tree(tree.write().unwrap()).unwrap();
        let old = git
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Old content",
                &tree,
                &[&parent],
            )
            .unwrap();
        let parent = git.find_commit(old).unwrap();
        let mut tree = git.treebuilder(Some(&tree)).unwrap();
        tree.remove("removed-secret.txt").unwrap();
        let tree = git.find_tree(tree.write().unwrap()).unwrap();
        git.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Current content",
            &tree,
            &[&parent],
        )
        .unwrap();
        git.reference("refs/heads/dev", old, true, "Fixture branch")
            .unwrap();
        git.tag_lightweight("v1", &git.find_object(old, None).unwrap(), false)
            .unwrap();
        old
    };
    let old_blob = {
        let git = git2::Repository::open_bare(&source.disk_path).unwrap();
        let commit = git.find_commit(old).unwrap();
        let tree = commit.tree().unwrap();
        let entry = tree.get_name("removed-secret.txt").unwrap();
        entry.id()
    };
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        source.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let base = "/api/v3/repos/alice/template";
    checked(
        &router,
        "repos/create-fork",
        "POST",
        &format!("{base}/forks"),
        Some(&bob_reader),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/create-fork",
        "POST",
        &format!("{base}/forks"),
        Some(&outsider),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "repos/create-fork",
        "POST",
        &format!("{base}/forks"),
        Some(&writer),
        Value::Null,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let (_, fork) = checked(
        &router,
        "repos/create-fork",
        "POST",
        &format!("{base}/forks"),
        Some(&bob_writer),
        json!({"name":"fork","default_branch_only":true}),
        StatusCode::ACCEPTED,
    )
    .await;
    assert_eq!(fork["fork"], true);
    assert_eq!(fork["private"], true);
    assert_eq!(fork["parent"]["id"], source.id);
    let fork_repo = f.repository(fork["id"].as_i64().unwrap()).await;
    {
        let git = git2::Repository::open_bare(&fork_repo.disk_path).unwrap();
        assert!(git.find_commit(old).is_ok());
        assert!(git.find_reference("refs/heads/dev").is_err());
        assert!(git.find_reference("refs/tags/v1").is_err());
    }
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{base}/forks"))
                .header("authorization", &bob_writer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let existing: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_github_schema("repos/create-fork", 202, &existing);
    assert_eq!(existing["id"], fork["id"]);
    checked(
        &router,
        "repos/create-using-template",
        "POST",
        &format!("{base}/generate"),
        Some(&bob_reader),
        json!({"name":"denied"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/create-using-template",
        "POST",
        &format!("{base}/generate"),
        Some(&outsider),
        json!({"name":"denied"}),
        StatusCode::NOT_FOUND,
    )
    .await;
    let (_, generated) = checked(
        &router,
        "repos/create-using-template",
        "POST",
        &format!("{base}/generate"),
        Some(&bob_writer),
        json!({"name":"generated","description":"From template"}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(generated["fork"], false);
    assert_eq!(generated["is_template"], false);
    assert_eq!(generated["private"], false);
    assert_eq!(generated["description"], "From template");
    let generated = f.repository(generated["id"].as_i64().unwrap()).await;
    {
        let git = git2::Repository::open_bare(&generated.disk_path).unwrap();
        let head = git.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.parent_count(), 0);
        assert_eq!(head.author().name(), Some("bob"));
        assert!(head.tree().unwrap().get_name("README.md").is_some());
        assert!(head
            .tree()
            .unwrap()
            .get_name("removed-secret.txt")
            .is_none());
        assert!(
            git.find_commit(old).is_err(),
            "Template history must be physically removed"
        );
        assert!(
            git.find_blob(old_blob).is_err(),
            "Old private file objects must be removed too"
        );
        assert!(git.find_reference("refs/heads/dev").is_err());
    }
    let (_, all) = checked(
        &router,
        "repos/create-using-template",
        "POST",
        &format!("{base}/generate"),
        Some(&bob_writer),
        json!({"name":"all-branches","include_all_branches":true,"private":true}),
        StatusCode::CREATED,
    )
    .await;
    let all = f.repository(all["id"].as_i64().unwrap()).await;
    {
        let git = git2::Repository::open_bare(&all.disk_path).unwrap();
        let main = git
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let dev = git
            .find_reference("refs/heads/dev")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        assert_eq!(main.parent_count(), 0);
        assert_eq!(dev.parent_count(), 0);
        assert_ne!(main.id(), dev.id());
        assert!(dev.tree().unwrap().get_name("removed-secret.txt").is_some());
        assert!(git.find_commit(old).is_err());
        assert!(git.find_reference("refs/tags/v1").is_err());
    }
    let input = db::organizations::profiles::NewOrganization::new(
        "acme",
        db::organizations::profiles::OrganizationProfile::new(None, None).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        db::organizations::profiles::create_organization(f.pool(), alice, &input)
            .await
            .unwrap(),
        db::organizations::OrganizationOutcome::Applied(_)
    ));
    checked(
        &router,
        "repos/create-fork",
        "POST",
        &format!("{base}/forks"),
        Some(&bob_writer),
        json!({"organization":"acme"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    let (_, org) = checked(
        &router,
        "repos/create-fork",
        "POST",
        &format!("{base}/forks"),
        Some(&writer),
        json!({"organization":"acme","name":"org-fork"}),
        StatusCode::ACCEPTED,
    )
    .await;
    assert_eq!(org["full_name"], "acme/org-fork");
    assert_eq!(org["owner"]["type"], "Organization");
    let (_, again) = checked(
        &router,
        "repos/create-fork",
        "POST",
        &format!("{base}/forks"),
        Some(&writer),
        json!({"organization":"acme"}),
        StatusCode::ACCEPTED,
    )
    .await;
    assert_eq!(again["id"], org["id"]);
    let (_, org_template) = checked(
        &router,
        "repos/create-using-template",
        "POST",
        &format!("{base}/generate"),
        Some(&writer),
        json!({"owner":"acme","name":"org-template"}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(org_template["full_name"], "acme/org-template");
    assert_eq!(org_template["fork"], false);
    let native_owner = Client::login(&router, f.pool(), alice).await;
    let native_reader = Client::login(&router, f.pool(), bob).await;
    let page = native_reader.page_ok("/new?source=alice%2Ftemplate").await;
    assert!(page.contains("name=\"source\""));
    assert!(page.contains("alice/template"));
    assert!(native_reader
        .page_ok("/alice/template")
        .await
        .contains("Use this template"));
    let reply = native_reader
        .form_reply(
            "/new",
            &[
                ("name", "native-copy"),
                ("description", "Native copy"),
                ("visibility", "public"),
                ("source", "alice/template"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert_eq!(reply.location.as_deref(), Some("/bob/native-copy"));
    checked(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/bob/native-copy",
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let reply = native_reader
        .form_reply(
            "/alice/template/_forms/template",
            &[("repository_id", &source.id.to_string())],
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let (_, fresh) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"fresh-source","auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let org_model = db::organizations::profiles::get_public_organization(f.pool(), "acme")
        .await
        .unwrap()
        .unwrap();
    let reply = native_owner
        .form_reply(
            "/new",
            &[
                ("name", "native-org-fork"),
                ("visibility", "public"),
                ("source", "alice/fresh-source"),
                ("fork_source", "on"),
                ("owner_org_id", &org_model.id.to_string()),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert_eq!(reply.location.as_deref(), Some("/acme/native-org-fork"));
    let (_, native_fork) = checked(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/native-org-fork",
        Some(&writer),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(native_fork["parent"]["id"], fresh["id"]);
    let reply = native_owner
        .form_reply(
            "/alice/template/_forms/template",
            &[("repository_id", &source.id.to_string())],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert!(!f.repository(source.id).await.is_template);
    let reply = native_owner
        .form_reply(
            "/alice/template/_forms/template",
            &[
                ("repository_id", &source.id.to_string()),
                ("is_template", "on"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert!(f.repository(source.id).await.is_template);
    checked(
        &router,
        "repos/update",
        "PATCH",
        base,
        Some(&writer),
        json!({"is_template":false}),
        StatusCode::OK,
    )
    .await;
    checked(
        &router,
        "repos/create-using-template",
        "POST",
        &format!("{base}/generate"),
        Some(&bob_writer),
        json!({"name":"not-a-template"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
}

#[tokio::test]
async fn sqlite_github_forks_and_templates() {
    copies(false).await;
}
#[tokio::test]
async fn postgres_github_forks_and_templates() {
    copies(true).await;
}

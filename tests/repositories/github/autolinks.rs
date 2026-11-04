use super::*;

async fn autolinks_contract(pg: bool) {
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
        json!({"name":"links","private":true,"auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let repo = f.repository(created["id"].as_i64().unwrap()).await;
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        codeyard_db_core::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let base = "/api/v3/repos/alice/links/autolinks";
    let (_,ticket)=checked(&router,"repos/create-autolink","POST",base,Some(&writer),json!({"key_prefix":"TKT-","url_template":"https://tickets.test/<num>","is_alphanumeric":false}),StatusCode::CREATED).await;
    let id = ticket["id"].as_i64().unwrap();
    let (_, alpha) = checked(
        &router,
        "repos/create-autolink",
        "POST",
        base,
        Some(&writer),
        json!({"key_prefix":"ABC-","url_template":"https://alpha.test/<num>"}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(alpha["is_alphanumeric"], true);
    checked(
        &router,
        "repos/create-autolink",
        "POST",
        base,
        Some(&reader),
        json!({"key_prefix":"READ-","url_template":"https://x.test/<num>"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    for body in [
        json!({"key_prefix":"TKT-","url_template":"https://x.test/<num>"}),
        json!({"key_prefix":"bad space","url_template":"https://x.test/<num>"}),
        json!({"key_prefix":"JS-","url_template":"javascript:alert(<num>)"}),
        json!({"key_prefix":"CRED-","url_template":"https://user:password@x.test/<num>"}),
        json!({"key_prefix":"NONE-","url_template":"https://x.test/no-placeholder"}),
    ] {
        checked(
            &router,
            "repos/create-autolink",
            "POST",
            base,
            Some(&writer),
            body,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let (headers, rows) = checked(
        &router,
        "repos/list-autolinks",
        "GET",
        &format!("{base}?per_page=1"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    checked(
        &router,
        "repos/get-autolink",
        "GET",
        &format!("{base}/{id}"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    checked(
        &router,
        "repos/list-autolinks",
        "GET",
        base,
        Some(&outsider),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/list-autolinks",
        "GET",
        base,
        None,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"other"}),
        StatusCode::CREATED,
    )
    .await;
    checked(
        &router,
        "repos/get-autolink",
        "GET",
        &format!("/api/v3/repos/alice/other/autolinks/{id}"),
        Some(&reader),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    checked(
        &router,
        "repos/delete-autolink",
        "DELETE",
        &format!("/api/v3/repos/alice/other/autolinks/{id}"),
        Some(&writer),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        let parent = git.head().unwrap().peel_to_commit().unwrap();
        let tree = parent.tree().unwrap();
        let mut builder = git.treebuilder(Some(&tree)).unwrap();
        builder
            .insert(
                "README.md",
                git.blob(b"TKT-12 abc-a1 `TKT-99`\n").unwrap(),
                0o100644,
            )
            .unwrap();
        let tree = git.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
        git.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Autolink text",
            &tree,
            &[&parent],
        )
        .unwrap();
    }
    let native_reader = Client::login(&router, f.pool(), bob).await;
    let page = native_reader.page_ok("/alice/links").await;
    assert!(page.contains("href=\"https://tickets.test/12\""));
    assert!(page.contains("href=\"https://alpha.test/a1\""));
    assert!(!page.contains("href=\"https://tickets.test/99\""));
    let owner = Client::login(&router, f.pool(), alice).await;
    assert!(owner
        .page_ok("/alice/links/settings?tab=autolinks")
        .await
        .contains("Autolink references"));
    let reply = owner
        .form_reply(
            "/alice/links/_forms/autolink-add",
            &[
                ("repository_id", &repo.id.to_string()),
                ("key_prefix", "N-"),
                ("url_template", "https://native.test/<num>"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let (_, rows) = checked(
        &router,
        "repos/list-autolinks",
        "GET",
        base,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let native = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["key_prefix"] == "N-")
        .unwrap();
    assert_eq!(native["is_alphanumeric"], false);
    let reply = owner
        .form_reply(
            "/alice/links/_forms/autolink-delete",
            &[
                ("repository_id", &repo.id.to_string()),
                ("autolink_id", &native["id"].to_string()),
                ("confirm", "yes"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    checked(
        &router,
        "repos/delete-autolink",
        "DELETE",
        &format!("{base}/{id}"),
        Some(&reader),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/delete-autolink",
        "DELETE",
        &format!("{base}/{id}"),
        Some(&writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    for index in 0..49 {
        let input = codeyard_db_repos::repositories::autolinks::NewAutolink::new(
            &format!("F{index}-"),
            "https://fill.test/<num>",
            false,
        )
        .unwrap();
        codeyard_db_repos::repositories::autolinks::create(f.pool(), alice, repo.id, &input)
            .await
            .unwrap();
    }
    checked(
        &router,
        "repos/create-autolink",
        "POST",
        base,
        Some(&writer),
        json!({"key_prefix":"EXTRA-","url_template":"https://x.test/<num>"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/alice/links",
        Some(&writer),
        json!({"archived":true}),
        StatusCode::OK,
    )
    .await;
    checked(
        &router,
        "repos/delete-autolink",
        "DELETE",
        &format!("{base}/{}", alpha["id"]),
        Some(&writer),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/list-autolinks",
        "GET",
        base,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
}
#[tokio::test]
async fn sqlite_github_autolinks() {
    autolinks_contract(false).await;
}
#[tokio::test]
async fn postgres_github_autolinks() {
    autolinks_contract(true).await;
}

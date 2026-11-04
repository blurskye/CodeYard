use super::*;

async fn pages(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("package-profile").await;
    let bob = f.actor("package-visitor").await;
    f.execute("INSERT INTO repositories(owner_user_id,name,disk_path,is_private) VALUES($1,'library','packages-library.git',true)",&[alice]).await;
    let repo = f
        .scalar(
            "SELECT id FROM repositories WHERE owner_user_id=$1 AND name='library'",
            &[alice],
        )
        .await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = format!("Bearer {}", token(&f, &router, alice, "write").await);
    rest::publish(&router, &writer, "package-profile", "library", "1.0.0").await;
    let owner = Client::login(&router, f.pool(), alice).await;
    let visitor = Client::login(&router, f.pool(), bob).await;
    let anonymous = Client::anonymous(&router);
    let path = "/users/package-profile/packages/npm/library";
    let settings = format!("{path}/settings");
    let html = owner.page_ok(path).await;
    assert!(
        html.contains("npm install @package-profile/library@1.0.0"),
        "{html}"
    );
    assert!(html.contains("/api/packages/package-profile/npm/"));
    assert!(!html.contains(&writer));
    assert_eq!(visitor.page(path).await.0, StatusCode::NOT_FOUND);
    assert_eq!(anonymous.page(path).await.0, StatusCode::NOT_FOUND);
    let html = owner.page_ok(&settings).await;
    assert!(html.contains("Package settings"));
    let package = f
        .scalar("SELECT id FROM packages WHERE repo_id=$1", &[repo])
        .await;
    let package_id = package.to_string();
    let fields = [
        ("action", "settings"),
        ("visibility", "public"),
        ("repository", "library"),
        ("package_id", package_id.as_str()),
    ];
    let reply = owner.form_reply(&settings, &fields).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert_eq!(reply.location.as_deref(), Some(settings.as_str()));
    // Making a package public does not expose a private linked repository's contents.
    assert_eq!(anonymous.page(path).await.0, StatusCode::NOT_FOUND);
    let reply = visitor.form_reply(&settings, &fields).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&settings)
                .header("cookie", format!("session_id={}", owner.session))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "action=settings&visibility=public&repository=library&package_id={package}"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // Detaching needs package administration and makes public registry reads available.
    let reply = owner
        .form_reply(
            &settings,
            &[
                ("action", "settings"),
                ("visibility", "public"),
                ("repository", ""),
                ("package_id", &package_id),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(anonymous.page(path).await.0, StatusCode::OK);
    let html = anonymous.page_ok("/explore/packages?type=npm").await;
    assert!(html.contains("package-profile / library"));
    let html = owner
        .page_ok("/users/package-profile/packages?type=npm")
        .await;
    assert!(html.contains("Packages"));
    assert!(html.contains("library"));
    let (status, _, bytes) = request(
        &router,
        "GET",
        "/api/packages/package-profile/npm/@package-profile%2flibrary",
        None,
        "application/json",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["dist-tags"]["latest"],
        "1.0.0"
    );
    let version = f
        .scalar(
            "SELECT id FROM package_versions WHERE package_id=$1",
            &[package],
        )
        .await
        .to_string();
    let html = anonymous
        .page_ok(&format!("{path}/versions/{version}"))
        .await;
    assert!(html.contains("Version details"));
    assert!(html.contains("1.0.0"));
    assert!(html.contains("sha256:"));
    let reply = owner
        .form_reply(
            &settings,
            &[
                ("action", "delete-version"),
                ("version_id", &version),
                ("package_id", &package_id),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let html = owner.page_ok(&settings).await;
    assert!(html.contains("Restore"));
    let reply = owner
        .form_reply(
            &settings,
            &[
                ("action", "restore-version"),
                ("version_id", &version),
                ("package_id", &package_id),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let reply = owner
        .form_reply(
            &settings,
            &[
                ("action", "delete"),
                ("confirm", "wrong"),
                ("package_id", &package_id),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    let reply = owner
        .form_reply(
            &settings,
            &[
                ("action", "delete"),
                ("confirm", "library"),
                ("package_id", &package_id),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.location.as_deref(),
        Some("/users/package-profile/packages")
    );
    assert_eq!(owner.page(path).await.0, StatusCode::NOT_FOUND);
    // Recovery is bounded to 30 days, after which collection removes versions and objects.
    f.execute("UPDATE packages SET deleted_at=1 WHERE id=$1", &[package])
        .await;
    f.execute("UPDATE package_blobs SET expires_at=0", &[])
        .await;
    f.execute("UPDATE package_blob_grants SET expires_at=0", &[])
        .await;
    let auth = tokens::token_hash(writer.trim_start_matches("Bearer "));
    let token = codeyard_db_identity::users::tokens::find_token(f.pool(), &auth)
        .await
        .unwrap()
        .unwrap();
    let error =
        codeyard_db_repos::packages::set_deleted(f.pool(), package, None, token.id.into(), true)
            .await
            .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<codeyard_db_repos::packages::Refusal>(),
        Some(codeyard_db_repos::packages::Refusal::Missing)
    ));
    let storage = codeyard::backend::storage::ObjectStore::open(root.path(), None)
        .await
        .unwrap();
    packages::collect(f.pool(), &storage).await.unwrap();
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM packages WHERE id=$1", &[package])
            .await,
        0
    );
    // The removed version's objects stay for the backup restore window, then go.
    assert_eq!(f.scalar("SELECT COUNT(*) FROM package_blobs", &[]).await, 1);
    f.execute("UPDATE package_blobs SET expires_at=0", &[])
        .await;
    packages::collect(f.pool(), &storage).await.unwrap();
    assert_eq!(f.scalar("SELECT COUNT(*) FROM package_blobs", &[]).await, 0);
    f.finish().await;
}

#[tokio::test]
async fn packages_pages_sqlite() {
    pages(false).await;
}
#[tokio::test]
async fn packages_pages_postgres() {
    pages(true).await;
}

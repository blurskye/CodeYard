use super::*;
use axum::http::StatusCode;

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("provenance-page-owner").await;
    let bob = f.actor("provenance-page-visitor").await;
    f.execute(
        "INSERT INTO organizations(name) VALUES('provenance-pages')",
        &[],
    )
    .await;
    let org = f
        .scalar(
            "SELECT id FROM organizations WHERE name='provenance-pages'",
            &[],
        )
        .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
        &[org, alice],
    )
    .await;
    f.execute("INSERT INTO repositories(owner_kind,owner_org_id,name,disk_path,is_private) VALUES('org',$1,'public','/provenance-pages/public.git',false),('org',$1,'private','/provenance-pages/private.git',true)",&[org]).await;
    let private_repo = f
        .scalar(
            "SELECT id FROM repositories WHERE owner_org_id=$1 AND name='private'",
            &[org],
        )
        .await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = token(&f, &router, alice, "write").await;
    let reader = token(&f, &router, alice, "read").await;
    let outsider = token(&f, &router, bob, "write").await;
    let public_digest = format!("sha256:{}", "1".repeat(64));
    let private_digest = format!("sha256:{}", "2".repeat(64));
    let public_bundle = bundle(
        std::slice::from_ref(&public_digest),
        "https://example.test/public</script><script>alert('provenance')</script>",
        "public signature",
    );
    let private_bundle = bundle(
        std::slice::from_ref(&private_digest),
        "https://private.example.test/provenance",
        "private signature",
    );
    let (_, public) = call(
        &router,
        "repos/create-attestation",
        "POST",
        "/api/v3/repos/provenance-pages/public/attestations",
        Some(&writer),
        json!({"bundle":public_bundle}),
        201,
    )
    .await;
    let (_, private) = call(
        &router,
        "repos/create-attestation",
        "POST",
        "/api/v3/repos/provenance-pages/private/attestations",
        Some(&writer),
        json!({"bundle":private_bundle}),
        201,
    )
    .await;
    let owner = Client::login(&router, f.pool(), alice).await;
    let visitor = Client::login(&router, f.pool(), bob).await;
    let guest = Client::anonymous(&router);
    let base = "/orgs/provenance-pages/attestations";
    let html = owner.page_ok(base).await;
    assert!(html.contains("Download Sigstore bundle"));
    assert!(html.contains("signer identity"));
    assert!(html.contains("https://private.example.test/provenance"));
    assert!(!html.contains(&writer));
    let html = guest.page_ok(base).await;
    assert!(html.contains("https://example.test/public"));
    assert!(!html.contains("<script>alert('provenance')</script>"));
    assert!(!html.contains("https://private.example.test/provenance"));
    let (_, model) = owner.model(base).await;
    let codeyard::webui::app::Page::Attestations(page) = model else {
        panic!("typed attestation navigation")
    };
    assert_eq!(page.entries.len(), 2);
    assert!(page.repo.is_none());
    let (_, model) = owner.model("/provenance-pages/private/attestations").await;
    let codeyard::webui::app::Page::Attestations(page) = model else {
        panic!("repository attestation chrome")
    };
    assert_eq!(page.entries.len(), 1);
    assert!(page.repo.is_some());
    assert_eq!(
        guest.page("/provenance-pages/private/attestations").await.0,
        StatusCode::NOT_FOUND
    );
    let private_download = format!(
        "/provenance-pages/private/attestations/{}/bundle",
        private["id"]
    );
    let reply = owner
        .exchange("GET", &private_download, "application/json", vec![])
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&reply.body).unwrap(),
        private_bundle
    );
    assert_eq!(
        guest
            .exchange("GET", &private_download, "application/json", vec![])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let (_, listed) = call(
        &router,
        "repos/list-attestations",
        "GET",
        &format!("/api/v3/repos/provenance-pages/private/attestations/{private_digest}"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(listed["attestations"][0]["bundle"], private_bundle);
    let bundle_url =
        url::Url::parse(listed["attestations"][0]["bundle_url"].as_str().unwrap()).unwrap();
    assert_eq!(
        bundle_url.path(),
        format!(
            "/api/v3/repos/provenance-pages/private/attestations/bundles/{}",
            private["id"]
        )
    );
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(bundle_url.path())
                .header("authorization", format!("Bearer {reader}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        private_bundle
    );
    call(
        &router,
        "orgs/delete-attestations-by-id",
        "DELETE",
        &format!(
            "/api/v3/orgs/provenance-pages/attestations/{}",
            private["id"]
        ),
        Some(&outsider),
        Value::Null,
        404,
    )
    .await;
    let missing_csrf = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(base)
                .header("cookie", format!("session_id={}", owner.session))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!("attestation_id={}", private["id"])))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_csrf.status(), StatusCode::FORBIDDEN);
    let public_id = public["id"].to_string();
    let private_id = private["id"].to_string();
    assert_eq!(
        owner
            .form_reply(
                "/provenance-pages/private/attestations",
                &[("attestation_id", &public_id)]
            )
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        visitor
            .form_reply(base, &[("attestation_id", &private_id)])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let reply = owner
        .form_reply(base, &[("attestation_id", &private_id)])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert_eq!(reply.location.as_deref(), Some(base));
    codeyard::backend::db::attestations::collect(f.pool())
        .await
        .unwrap();
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM attestation_bundles", &[])
            .await,
        1
    );
    assert_eq!(
        owner
            .exchange("GET", &private_download, "application/json", vec![])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    f.execute(
        "UPDATE repositories SET is_private=true WHERE owner_org_id=$1 AND name='public'",
        &[org],
    )
    .await;
    let html = guest.page_ok(base).await;
    assert!(!html.contains("https://example.test/public"));
    f.execute(
        "UPDATE repositories SET is_private=false WHERE id=$1",
        &[private_repo],
    )
    .await;
    assert_eq!(
        guest
            .exchange("GET", &private_download, "application/json", vec![])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_attestation_pages() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_attestation_pages() {
    contract(true).await;
}

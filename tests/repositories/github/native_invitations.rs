use super::*;

async fn native_invitations(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let eve = f.actor("eve").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let bob_reader = credential(&router, &f, bob, "read").await;
    let eve_reader = credential(&router, &f, eve, "read").await;
    let owner = Client::login(&router, f.pool(), alice).await;
    let recipient = Client::login(&router, f.pool(), bob).await;
    let other = Client::login(&router, f.pool(), eve).await;
    let (_, repo) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"private-invitations","private":true,"auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let (_, invitation) = checked(
        &router,
        "repos/add-collaborator",
        "PUT",
        "/api/v3/repos/alice/private-invitations/collaborators/bob",
        Some(&writer),
        json!({"permission":"maintain"}),
        StatusCode::CREATED,
    )
    .await;
    let id = invitation["id"].as_i64().unwrap();
    checked(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/alice/private-invitations",
        Some(&bob_reader),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let page = recipient.page_ok("/settings/invitations").await;
    assert!(page.contains("private-invitations"));
    assert!(page.contains("Accept"));
    assert!(page.contains("Decline"));
    assert!(page.contains("maintain"));
    assert!(!page.contains(&writer));
    assert!(!other
        .page_ok("/settings/invitations")
        .await
        .contains("private-invitations"));
    let denied = other
        .form_reply(&format!("/settings/invitations/{id}/accept"), &[])
        .await;
    assert_eq!(denied.status, StatusCode::NOT_FOUND);
    let reply = recipient
        .form_reply(&format!("/settings/invitations/{id}/accept"), &[])
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let (_, accessible) = checked(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/alice/private-invitations",
        Some(&bob_reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(accessible["permissions"]["maintain"], true);
    assert_eq!(accessible["permissions"]["admin"], false);
    assert!(!recipient
        .page_ok("/settings/invitations")
        .await
        .contains("private-invitations"));
    let (_, invitation) = checked(
        &router,
        "repos/add-collaborator",
        "PUT",
        "/api/v3/repos/alice/private-invitations/collaborators/eve",
        Some(&writer),
        json!({"permission":"read"}),
        StatusCode::CREATED,
    )
    .await;
    let reply = other
        .form_reply(
            &format!("/settings/invitations/{}/decline", invitation["id"]),
            &[],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    checked(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/alice/private-invitations",
        Some(&eve_reader),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let (status, _) = Client::anonymous(&router)
        .page("/settings/invitations")
        .await;
    assert!(status.is_redirection());
    let input = codeyard_db_orgs::organizations::profiles::NewOrganization::new(
        "acme",
        codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        codeyard_db_orgs::organizations::profiles::create_organization(f.pool(), alice, &input)
            .await
            .unwrap(),
        codeyard_database::queries::organizations::OrganizationOutcome::Applied(_)
    ));
    let (_, org_repo) = checked(
        &router,
        "repos/create-in-org",
        "POST",
        "/api/v3/orgs/acme/repos",
        Some(&writer),
        json!({"name":"private","private":true}),
        StatusCode::CREATED,
    )
    .await;
    let page = owner
        .page_ok("/acme/private/settings?tab=collaborators")
        .await;
    assert!(page.contains("Invite collaborator"));
    assert!(!page.contains("Add collaborator"));
    assert!(page.contains("maintain"));
    let reply = owner
        .form_reply(
            "/acme/private/_forms/invite-collaborator",
            &[
                ("repository_id", &org_repo["id"].to_string()),
                ("username", "eve"),
                ("access", "triage"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let (_, pending) = checked(
        &router,
        "repos/list-invitations-for-authenticated-user",
        "GET",
        "/api/v3/user/repository_invitations",
        Some(&eve_reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let id = pending[0]["id"].as_i64().unwrap();
    let reply = other
        .form_reply(&format!("/settings/invitations/{id}/accept"), &[])
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let (_, accessible) = checked(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/private",
        Some(&eve_reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(accessible["permissions"]["triage"], true);
    assert_eq!(accessible["permissions"]["push"], false);
    let reply = owner
        .form_reply(
            "/acme/private/_forms/remove-invited-collaborator",
            &[
                ("repository_id", &org_repo["id"].to_string()),
                ("username", "eve"),
                ("confirm", "yes"),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    checked(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/private",
        Some(&eve_reader),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(repo["private"], true);
}
#[tokio::test]
async fn sqlite_github_native_invitation_workflow() {
    native_invitations(false).await;
}
#[tokio::test]
async fn postgres_github_native_invitation_workflow() {
    native_invitations(true).await;
}

use super::*;

async fn teams_contract(pg: bool) {
    use db::organizations::{membership, profiles, teams, OrganizationOutcome};
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let carol = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let reader = credential(&router, &f, alice, "read").await;
    let bob_reader = credential(&router, &f, bob, "read").await;
    let outsider = credential(&router, &f, carol, "read").await;
    let input = profiles::NewOrganization::new(
        "acme",
        profiles::OrganizationProfile::new(None, None).unwrap(),
    )
    .unwrap();
    let OrganizationOutcome::Applied(org) = profiles::create_organization(f.pool(), alice, &input)
        .await
        .unwrap()
    else {
        panic!("organization not created")
    };
    assert!(matches!(
        membership::add_organization_member(
            f.pool(),
            alice,
            org.id,
            bob,
            codeyard_db_core::models::OrgRole::Member
        )
        .await
        .unwrap(),
        OrganizationOutcome::Applied(_)
    ));
    let (_, repo) = checked(
        &router,
        "repos/create-in-org",
        "POST",
        "/api/v3/orgs/acme/repos",
        Some(&writer),
        json!({"name":"shared","private":true}),
        StatusCode::CREATED,
    )
    .await;
    let id = repo["id"].as_i64().unwrap();
    for (slug, privacy, permission) in [
        ("secret", teams::TeamPrivacy::Secret, 1),
        ("core", teams::TeamPrivacy::Visible, 4),
    ] {
        let input =
            teams::NewTeam::new(slug, teams::TeamProfile::new(slug, None, privacy).unwrap())
                .unwrap();
        let OrganizationOutcome::Applied(team) =
            teams::create_organization_team(f.pool(), alice, org.id, &input)
                .await
                .unwrap()
        else {
            panic!("team not created")
        };
        assert!(matches!(
            teams::grant_organization_team_repository(
                f.pool(),
                alice,
                org.id,
                team.id,
                id,
                teams::TeamRepositoryPermission::new(permission).unwrap()
            )
            .await
            .unwrap(),
            OrganizationOutcome::Applied(_)
        ));
    }
    for user in [bob, carol] {
        db::repositories::access::grant_repo_access(
            f.pool(),
            user,
            id,
            codeyard_db_core::models::AccessLevel::Admin,
        )
        .await
        .unwrap();
    }
    let (_, rows) = checked(
        &router,
        "repos/list-teams",
        "GET",
        "/api/v3/repos/acme/shared/teams",
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(rows[0]["privacy"], "secret");
    assert_eq!(rows[0]["permission"], "pull");
    assert_eq!(rows[1]["permission"], "maintain");
    let (headers, rows) = checked(
        &router,
        "repos/list-teams",
        "GET",
        "/api/v3/repos/acme/shared/teams?per_page=1",
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let (_, rows) = checked(
        &router,
        "repos/list-teams",
        "GET",
        "/api/v3/repos/acme/shared/teams?per_page=1&page=2",
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(rows[0]["slug"], "core");
    let (headers, rows) = checked(
        &router,
        "repos/list-teams",
        "GET",
        "/api/v3/repos/acme/shared/teams?per_page=1",
        Some(&bob_reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(rows[0]["slug"], "core");
    assert!(!headers.contains_key("link"));
    checked(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/shared",
        Some(&outsider),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    checked(
        &router,
        "repos/list-teams",
        "GET",
        "/api/v3/repos/acme/shared/teams",
        Some(&outsider),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/list-teams",
        "GET",
        "/api/v3/repos/acme/shared/teams",
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
        json!({"name":"personal"}),
        StatusCode::CREATED,
    )
    .await;
    let (_, rows) = checked(
        &router,
        "repos/list-teams",
        "GET",
        "/api/v3/repos/alice/personal/teams",
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(rows.as_array().unwrap().is_empty());
    let native = Client::login(&router, f.pool(), alice).await;
    assert!(native
        .page_ok("/orgs/acme/teams/secret?tab=repositories")
        .await
        .contains("shared"));
}
#[tokio::test]
async fn sqlite_github_repository_teams() {
    teams_contract(false).await;
}
#[tokio::test]
async fn postgres_github_repository_teams() {
    teams_contract(true).await;
}

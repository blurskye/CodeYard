use super::{
    request,
    roles::{repo, run},
};
use crate::support::{
    codeyard_db_orgs::organizations::applied, database::Database, http_client::Client,
};

use codeyard_db_orgs::organizations::NewOrganization;
use codeyard_db_orgs::organizations::OrganizationProfile;

use codeyard::backend::identity::tokens::TokenInput;
use codeyard_db_core::models::AccessLevel;
use codeyard_db_core::models::OrgRole;
use serde_json::{json, Value};
const UPDATE: &str = "orgs/update";
const GET: &str = "orgs/get";
mod native;
mod policies;
async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let delegated = f.actor("delegated").await;
    let outsider = f.actor("outside").await;
    let mut orgs = Vec::new();
    for name in ["acme", "foreign"] {
        orgs.push(applied(
            codeyard_db_orgs::organizations::create_organization(
                f.pool(),
                owner,
                &NewOrganization::new(name, OrganizationProfile::new(None, None).unwrap()).unwrap(),
            )
            .await
            .unwrap(),
        ));
    }
    for (user, role) in [(member, OrgRole::Member), (delegated, OrgRole::Admin)] {
        applied(
            codeyard_db_orgs::organizations::add_organization_member(
                f.pool(),
                owner,
                orgs[0].id,
                user,
                role,
            )
            .await
            .unwrap(),
        );
    }
    applied(
        codeyard_db_orgs::organizations::add_organization_member(
            f.pool(),
            owner,
            orgs[1].id,
            member,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let first = repo(&f, root.path(), owner, orgs[0].id, "first").await;
    let second = repo(&f, root.path(), owner, orgs[0].id, "second").await;
    let foreign = repo(&f, root.path(), owner, orgs[1].id, "foreign").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[outsider])
        .await;
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), owner).await;
    let mut credentials = Vec::new();
    for (user, scope) in [
        (owner, "write"),
        (owner, "read"),
        (member, "write"),
        (delegated, "write"),
        (outsider, "write"),
    ] {
        let session = Client::login(&router, f.pool(), user).await;
        let token = codeyard::backend::identity::tokens::create(
            f.pool(),
            user,
            Some(&session.session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let write = Some(credentials[0].as_str());
    let read = Some(credentials[1].as_str());
    let member_token = Some(credentials[2].as_str());
    let path = "/api/v3/orgs/acme";
    for (token, status) in [
        (None, 401),
        (read, 403),
        (member_token, 403),
        (Some(credentials[3].as_str()), 403),
        (Some(credentials[4].as_str()), 403),
    ] {
        request(
            &router,
            UPDATE,
            "PATCH",
            path,
            token,
            json!({"company":"Forbidden"}),
            status,
        )
        .await;
    }
    let (_,updated)=request(&router,UPDATE,"PATCH",path,write,json!({"name":"Acme","description":"Native and REST settings","company":"Acme & tools","email":"public@example.test","billing_email":"private-billing@example.test","blog":"https://example.test/tools","location":"Montréal","twitter_username":"acme"}),200).await;
    assert_eq!(updated["billing_email"], "private-billing@example.test");
    assert_eq!(updated["default_repository_permission"], "none");
    assert_eq!(updated["company"], "Acme & tools");
    assert_eq!(updated["has_organization_projects"], true);
    for token in [
        None,
        member_token,
        Some(credentials[3].as_str()),
        Some(credentials[4].as_str()),
    ] {
        let (_, public) = request(&router, GET, "GET", path, token, Value::Null, 200).await;
        assert!(public.get("billing_email").is_none());
        assert_eq!(public["email"], "public@example.test");
    }
    let (_, owned) = request(&router, GET, "GET", path, read, Value::Null, 200).await;
    assert_eq!(owned["billing_email"], "private-billing@example.test");
    let (_,cleared)=request(&router,UPDATE,"PATCH",path,write,json!({"company":null,"email":null,"billing_email":null,"twitter_username":null,"location":null,"blog":null}),200).await;
    for field in [
        "company",
        "email",
        "billing_email",
        "twitter_username",
        "location",
        "blog",
    ] {
        assert!(cleared[field].is_null(), "{field}");
    }
    assert_eq!(cleared["name"], "Acme");
    request(&router,UPDATE,"PATCH",path,write,json!({"company":"Acme & tools","email":"public@example.test","billing_email":"private-billing@example.test","blog":"https://example.test/tools","location":"Montréal","twitter_username":"acme"}),200).await;
    for patch in [
        json!({"name":"Should not save","default_repository_permission":"owner"}),
        json!({"members_can_create_repositories":"true"}),
        json!({"members_can_create_private_repositories":null}),
        json!({"email":"not an address"}),
        json!({"blog":"javascript:alert(1)"}),
        json!({"members_can_create_internal_repositories":true}),
        json!({"has_repository_projects":true}),
    ] {
        request(&router, UPDATE, "PATCH", path, write, patch, 422).await;
    }
    let (_, unchanged) = request(&router, GET, "GET", path, read, Value::Null, 200).await;
    assert_eq!(unchanged["name"], "Acme");
    assert_eq!(unchanged["company"], "Acme & tools");
    let _ = tokio::join!(
        request(
            &router,
            UPDATE,
            "PATCH",
            path,
            write,
            json!({"location":"Québec"}),
            200
        ),
        request(
            &router,
            UPDATE,
            "PATCH",
            path,
            write,
            json!({"company":"Concurrent company"}),
            200
        )
    );
    let (_, merged) = request(&router, GET, "GET", path, read, Value::Null, 200).await;
    assert_eq!(merged["location"], "Québec");
    assert_eq!(merged["company"], "Concurrent company");
    request(
        &router,
        UPDATE,
        "PATCH",
        path,
        write,
        json!({"default_repository_permission":"read"}),
        200,
    )
    .await;
    assert_eq!(
        codeyard_db_repos::repositories::access::get_user_repo_access(f.pool(), member, second.id)
            .await
            .unwrap(),
        AccessLevel::Read
    );
    assert_eq!(
        codeyard_db_repos::repositories::access::get_user_repo_access(
            f.pool(),
            outsider,
            second.id
        )
        .await
        .unwrap(),
        AccessLevel::None
    );
    assert_eq!(
        codeyard_db_repos::repositories::access::get_user_repo_access(f.pool(), member, foreign.id)
            .await
            .unwrap(),
        AccessLevel::None
    );
    let (_, visible) = request(
        &router,
        "repos/list-for-authenticated-user",
        "GET",
        "/api/v3/user/repos?affiliation=organization_member&visibility=private",
        member_token,
        Value::Null,
        200,
    )
    .await;
    assert!(visible
        .as_array()
        .unwrap()
        .iter()
        .any(|repo| repo["id"] == second.id));
    assert!(!visible
        .as_array()
        .unwrap()
        .iter()
        .any(|repo| repo["id"] == foreign.id));
    request(
        &router,
        UPDATE,
        "PATCH",
        path,
        write,
        json!({"default_repository_permission":"write"}),
        200,
    )
    .await;
    f.execute(
        "INSERT INTO repo_access(user_id,repo_id,access_level) VALUES($1,$2,3),($1,$3,3)",
        &[member, first.id, foreign.id],
    )
    .await;
    for repository in [&first, &second, &foreign] {
        run(&f, repository.id, member, "queued").await;
    }
    request(
        &router,
        UPDATE,
        "PATCH",
        path,
        write,
        json!({"default_repository_permission":"none"}),
        200,
    )
    .await;
    for (repository, pending) in [(&first, 1), (&second, 0), (&foreign, 1)] {
        assert_eq!(
            f.scalar(
                "SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND status='queued'",
                &[repository.id]
            )
            .await,
            pending
        );
    }
    // Even repository administration for all members does not confer organization ownership.
    request(
        &router,
        UPDATE,
        "PATCH",
        path,
        write,
        json!({"default_repository_permission":"admin"}),
        200,
    )
    .await;
    assert_eq!(
        codeyard_db_repos::repositories::access::get_user_repo_access(f.pool(), member, second.id)
            .await
            .unwrap(),
        AccessLevel::Admin
    );
    request(
        &router,
        UPDATE,
        "PATCH",
        path,
        member_token,
        json!({"billing_email":"forbidden@example.test"}),
        403,
    )
    .await;
    request(&router,UPDATE,"PATCH",path,write,json!({"default_repository_permission":"none","members_can_create_repositories":false,"members_allowed_repository_creation_type":"private"}),200).await;
    request(
        &router,
        "repos/create-in-org",
        "POST",
        "/api/v3/orgs/acme/repos",
        member_token,
        json!({"name":"refused-public","private":false}),
        403,
    )
    .await;
    let (_, created) = request(
        &router,
        "repos/create-in-org",
        "POST",
        "/api/v3/orgs/acme/repos",
        member_token,
        json!({"name":"member-created","private":true}),
        201,
    )
    .await;
    assert_eq!(
        codeyard_db_repos::repositories::access::get_user_repo_access(
            f.pool(),
            member,
            created["id"].as_i64().unwrap()
        )
        .await
        .unwrap(),
        AccessLevel::Admin
    );
    assert!(
        codeyard_db_repos::repositories::records::transfer_repository(
            f.pool(),
            member,
            created["id"].as_i64().unwrap(),
            codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(orgs[1].id)
        )
        .await
        .is_err(),
        "destination creation policy cannot be bypassed by transfer"
    );
    let child = codeyard_db_repos::repositories::records::get_repository_by_id(
        f.pool(),
        created["id"].as_i64().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(child.owner_org_id, Some(orgs[0].id));
    let target = codeyard_db_orgs::organizations::settings::Changes::parse(
        &json!({"members_can_create_private_repositories":true}),
    )
    .unwrap();
    applied(
        codeyard_db_orgs::organizations::settings::change(f.pool(), owner, orgs[1].id, &target)
            .await
            .unwrap(),
    );
    let moved = codeyard_db_repos::repositories::records::transfer_repository(
        f.pool(),
        member,
        child.id,
        codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(orgs[1].id),
    )
    .await
    .unwrap();
    assert_eq!(moved.owner_org_id, Some(orgs[1].id));
    assert_eq!(
        codeyard_db_repos::repositories::access::get_user_repo_access(f.pool(), member, child.id)
            .await
            .unwrap(),
        AccessLevel::Admin
    );
    let project = codeyard_db_repos::projects::create(
        f.pool(),
        owner,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&browser.session),
        codeyard_db_repos::projects::Owner::Organization(orgs[0].id),
        "Roadmap",
        None,
        true,
    )
    .await
    .unwrap();
    request(
        &router,
        UPDATE,
        "PATCH",
        path,
        write,
        json!({"has_organization_projects":false}),
        200,
    )
    .await;
    assert!(codeyard_db_repos::projects::list(
        f.pool(),
        None,
        &codeyard_db_repos::projects::ListFilter {
            owner: Some(codeyard_db_repos::projects::Owner::Organization(orgs[0].id)),
            ..Default::default()
        },
        0,
        50,
    )
    .await
    .unwrap()
    .1
    .is_empty());
    assert!(codeyard_db_repos::projects::get(
        f.pool(),
        Some(owner),
        codeyard_db_repos::projects::Owner::Organization(orgs[0].id),
        project.number
    )
    .await
    .is_err());
    assert!(codeyard_db_repos::projects::create(
        f.pool(),
        owner,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&browser.session),
        codeyard_db_repos::projects::Owner::Organization(orgs[0].id),
        "Refused",
        None,
        true
    )
    .await
    .is_err());
    request(
        &router,
        UPDATE,
        "PATCH",
        path,
        write,
        json!({"has_organization_projects":true}),
        200,
    )
    .await;
    assert_eq!(
        codeyard_db_repos::projects::get(
            f.pool(),
            Some(owner),
            codeyard_db_repos::projects::Owner::Organization(orgs[0].id),
            project.number
        )
        .await
        .unwrap()
        .id,
        project.id
    );
    native::contract(&f, &router, &browser, owner, orgs[0].id, &first, write).await;
    policies::contract(
        &f,
        &router,
        &browser,
        owner,
        &orgs,
        (&first, &second),
        write,
    )
    .await;
    f.finish().await;
}
#[tokio::test]
async fn github_organization_settings_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn github_organization_settings_postgres() {
    contract(true).await;
}

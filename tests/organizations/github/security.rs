use super::request;
use super::roles::repo;
use super::roles::run;
use crate::support::database::Database;
use crate::support::http_client::Client;
use crate::support::organizations::applied;
use axum::http::StatusCode;

use super::NewOrganization;
use super::NewTeam;
use super::OrganizationProfile;
use super::TeamPrivacy;
use super::TeamProfile;
use super::TeamRole;
use codeyard_db_core::queries::organizations::roles::SECURITY_MANAGER_ROLE_ID;

use codeyard_db_core::models::AccessLevel;
use codeyard_db_core::models::OrgRole;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
const LIST: &str = "orgs/list-security-manager-teams";
const ADD: &str = "orgs/add-security-manager-team";
const REMOVE: &str = "orgs/remove-security-manager-team";
async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let delegated = f.actor("delegated").await;
    let outsider = f.actor("outside").await;
    let mut orgs = vec![];
    for slug in ["acme", "foreign"] {
        orgs.push(applied(
            super::create_organization(
                f.pool(),
                owner,
                &NewOrganization::new(slug, OrganizationProfile::new(None, None).unwrap()).unwrap(),
            )
            .await
            .unwrap(),
        ));
    }
    for (id, role) in [(member, OrgRole::Member), (delegated, OrgRole::Admin)] {
        applied(
            super::add_organization_member(f.pool(), owner, orgs[0].id, id, role)
                .await
                .unwrap(),
        );
    }
    applied(
        super::add_organization_member(f.pool(), owner, orgs[1].id, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    let parent = applied(
        super::create_organization_team(
            f.pool(),
            owner,
            orgs[0].id,
            &NewTeam::new(
                "managers",
                TeamProfile::new("Managers", None, TeamPrivacy::Visible).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let child = applied(
        super::create_organization_team(
            f.pool(),
            owner,
            orgs[0].id,
            &NewTeam::new(
                "child",
                TeamProfile::new("Child", None, TeamPrivacy::Visible)
                    .unwrap()
                    .with_parent(Some(Some(parent.id)))
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let secret = applied(
        super::create_organization_team(
            f.pool(),
            owner,
            orgs[0].id,
            &NewTeam::new(
                "secret",
                TeamProfile::new("Secret", None, TeamPrivacy::Secret).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        super::create_organization_team(
            f.pool(),
            owner,
            orgs[1].id,
            &NewTeam::new(
                "foreign-only",
                TeamProfile::new("Foreign", None, TeamPrivacy::Visible).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        super::add_organization_team_member(
            f.pool(),
            owner,
            orgs[0].id,
            child.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let first = repo(&f, root.path(), owner, orgs[0].id, "first").await;
    let second = repo(&f, root.path(), owner, orgs[0].id, "second").await;
    let foreign = repo(&f, root.path(), owner, orgs[1].id, "foreign").await;
    f.execute(
        "INSERT INTO repo_access(user_id,repo_id,access_level) VALUES($1,$2,3)",
        &[member, first.id],
    )
    .await;
    run(&f, first.id, member, "queued").await;
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), owner).await;
    let mut credentials = vec![];
    for (id, scope) in [
        (owner, "write"),
        (owner, "read"),
        (member, "write"),
        (delegated, "write"),
        (outsider, "write"),
    ] {
        let session = Client::login(&router, f.pool(), id).await;
        let token = codeyard_identity::tokens::create(
            f.pool(),
            id,
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
    let member_auth = Some(credentials[2].as_str());
    let path = "/api/v3/orgs/acme/security-managers";
    let managers = format!("{path}/teams/managers");
    for (auth, status) in [
        (None, 401),
        (member_auth, 403),
        (Some(credentials[3].as_str()), 403),
        (Some(credentials[4].as_str()), 403),
    ] {
        request(&router, LIST, "GET", path, auth, Value::Null, status).await;
        request(&router, ADD, "PUT", &managers, auth, Value::Null, status).await;
        request(
            &router,
            REMOVE,
            "DELETE",
            &managers,
            auth,
            Value::Null,
            status,
        )
        .await;
    }
    request(&router, ADD, "PUT", &managers, read, Value::Null, 403).await;
    let (_, empty) = request(&router, LIST, "GET", path, read, Value::Null, 200).await;
    assert_eq!(empty, json!([]));
    request(
        &router,
        ADD,
        "PUT",
        &format!("{path}/teams/foreign-only"),
        write,
        Value::Null,
        404,
    )
    .await;
    for _ in 0..2 {
        request(&router, ADD, "PUT", &managers, write, Value::Null, 204).await;
    }
    let (_, teams) = request(&router, LIST, "GET", path, read, Value::Null, 200).await;
    assert_eq!(teams.as_array().unwrap().len(), 1);
    assert_eq!(teams[0]["id"], parent.id);
    assert!(codeyard_db_orgs::organizations::roles::can_manage_security(
        f.pool(),
        owner,
        orgs[0].id
    )
    .await
    .unwrap());
    assert!(codeyard_db_orgs::organizations::roles::can_manage_security(
        f.pool(),
        member,
        orgs[0].id
    )
    .await
    .unwrap());
    assert!(
        !codeyard_db_orgs::organizations::roles::can_manage_security(
            f.pool(),
            delegated,
            orgs[0].id
        )
        .await
        .unwrap()
    );
    assert!(
        !codeyard_db_orgs::organizations::roles::can_manage_security(f.pool(), member, orgs[1].id)
            .await
            .unwrap()
    );
    assert_eq!(
        codeyard_db_repos::repositories::access::get_user_repo_access(f.pool(), member, second.id)
            .await
            .unwrap(),
        AccessLevel::Read
    );
    assert_eq!(
        codeyard_db_repos::repositories::access::get_user_repo_access(f.pool(), member, foreign.id)
            .await
            .unwrap(),
        AccessLevel::None
    );
    // The legacy and replacement APIs observe the exact same assignment rows.
    let role_path =
        format!("/api/v3/orgs/acme/organization-roles/{SECURITY_MANAGER_ROLE_ID}/teams");
    let (_, role_teams) = request(
        &router,
        "orgs/list-org-role-teams",
        "GET",
        &role_path,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(role_teams[0]["id"], parent.id);
    let (_, role) = request(
        &router,
        "orgs/get-org-role",
        "GET",
        &format!("/api/v3/orgs/acme/organization-roles/{SECURITY_MANAGER_ROLE_ID}"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(role["base_role"], "read");
    assert!(role["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|permission| permission == "manage_security"));
    request(
        &router,
        "orgs/assign-team-to-org-role",
        "PUT",
        "/api/v3/orgs/acme/organization-roles/teams/child/1",
        write,
        Value::Null,
        204,
    )
    .await;
    for _ in 0..2 {
        request(
            &router,
            REMOVE,
            "DELETE",
            &managers,
            write,
            Value::Null,
            204,
        )
        .await;
    }
    assert!(
        !codeyard_db_orgs::organizations::roles::can_manage_security(f.pool(), member, orgs[0].id)
            .await
            .unwrap()
    );
    assert_eq!(
        codeyard_db_repos::repositories::access::get_user_repo_access(f.pool(), member, second.id)
            .await
            .unwrap(),
        AccessLevel::Read,
        "the independent read role survives"
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM ci_runs WHERE triggered_by_id=$1 AND status='queued'",
            &[member]
        )
        .await,
        1,
        "removing a read/security role must not cancel independently authorized CI"
    );
    request(&router, REMOVE, "DELETE", &managers, read, Value::Null, 403).await;
    let native = format!(
        "/settings/organizations/{}?tab=roles&role={SECURITY_MANAGER_ROLE_ID}",
        orgs[0].id
    );
    let (status, html) = browser.page(&native).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Security manager"));
    let response = browser
        .form_reply(
            &native,
            &[
                ("action", "assign-team-role"),
                ("team_slug", "managers"),
                ("role_id", "6"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let (_, teams) = request(&router, LIST, "GET", path, read, Value::Null, 200).await;
    assert_eq!(teams[0]["id"], parent.id);
    let response = browser
        .form_reply(
            &native,
            &[
                ("action", "revoke-team-role"),
                ("team_id", &parent.id.to_string()),
                ("role_id", "6"),
                ("confirm", "true"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(
        !codeyard_db_orgs::organizations::roles::can_manage_security(f.pool(), member, orgs[0].id)
            .await
            .unwrap()
    );
    // A security-only team grants reads and security management, not Git writes or CI authority.
    applied(
        super::add_organization_team_member(
            f.pool(),
            owner,
            orgs[0].id,
            secret.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    request(
        &router,
        ADD,
        "PUT",
        &format!("{path}/teams/secret"),
        write,
        Value::Null,
        204,
    )
    .await;
    assert!(codeyard_db_orgs::organizations::roles::can_manage_security(
        f.pool(),
        member,
        orgs[0].id
    )
    .await
    .unwrap());
    assert_eq!(
        codeyard_db_repos::repositories::access::get_user_repo_access(f.pool(), member, second.id)
            .await
            .unwrap(),
        AccessLevel::Read
    );
    applied(
        super::remove_organization_team_member(f.pool(), owner, orgs[0].id, secret.id, member)
            .await
            .unwrap(),
    );
    assert!(
        !codeyard_db_orgs::organizations::roles::can_manage_security(f.pool(), member, orgs[0].id)
            .await
            .unwrap()
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM ci_runs WHERE triggered_by_id=$1 AND status='queued'",
            &[member]
        )
        .await,
        1
    );
    applied(
        super::add_organization_team_member(
            f.pool(),
            owner,
            orgs[0].id,
            secret.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        super::delete_organization_team(f.pool(), owner, orgs[0].id, secret.id)
            .await
            .unwrap(),
    );
    assert!(
        !codeyard_db_orgs::organizations::roles::can_manage_security(f.pool(), member, orgs[0].id)
            .await
            .unwrap()
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM ci_runs WHERE triggered_by_id=$1 AND status='queued'",
            &[member]
        )
        .await,
        1
    );
    request(
        &router,
        "orgs/assign-user-to-org-role",
        "PUT",
        "/api/v3/orgs/acme/organization-roles/users/member/6",
        write,
        Value::Null,
        204,
    )
    .await;
    assert!(codeyard_db_orgs::organizations::roles::can_manage_security(
        f.pool(),
        member,
        orgs[0].id
    )
    .await
    .unwrap());
    let (_, teams) = request(&router, LIST, "GET", path, read, Value::Null, 200).await;
    assert_eq!(
        teams,
        json!([]),
        "direct user roles are not fabricated team assignments"
    );
    f.execute(
        "UPDATE users SET must_change_password=TRUE WHERE id=$1",
        &[member],
    )
    .await;
    assert!(
        !codeyard_db_orgs::organizations::roles::can_manage_security(f.pool(), member, orgs[0].id)
            .await
            .unwrap()
    );
    f.execute(
        "UPDATE users SET must_change_password=FALSE WHERE id=$1",
        &[member],
    )
    .await;
    f.execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[owner])
        .await;
    assert!(
        !codeyard_db_orgs::organizations::roles::can_manage_security(f.pool(), member, orgs[0].id)
            .await
            .unwrap(),
        "suspended governance cannot authorize security changes"
    );
    f.finish().await;
}
#[tokio::test]
async fn github_security_manager_teams_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn github_security_manager_teams_postgres() {
    contract(true).await;
}

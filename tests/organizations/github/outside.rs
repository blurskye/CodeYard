use super::request;
use crate::support::database::Database;
use crate::support::http_client::Client;
use crate::support::organizations::applied;
use axum::http::StatusCode;

use codeyard_db_orgs::organizations::profiles::NewOrganization;
use codeyard_db_orgs::organizations::profiles::OrganizationProfile;
use codeyard_db_orgs::organizations::teams::NewTeam;
use codeyard_db_orgs::organizations::teams::TeamPrivacy;
use codeyard_db_orgs::organizations::teams::TeamProfile;
use codeyard_db_orgs::organizations::teams::TeamRepositoryPermission;
use codeyard_db_orgs::organizations::teams::TeamRole;
use codeyard_db_repos::repositories::creation::NewRepository;
use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
use codeyard_db_repos::repositories::creation::RepositoryTarget;

use codeyard_database::models::AccessLevel;
use codeyard_database::models::OrgRole;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use std::time::Duration;

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let delegated = f.actor("delegated").await;
    let asynchronous = f.actor("asynchronous").await;
    let outsider = f.actor("outside").await;
    let secure = f.actor("secure").await;
    let org = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let foreign = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("foreign", OrganizationProfile::new(None, None).unwrap())
                .unwrap(),
        )
        .await
        .unwrap(),
    );
    for (id, role) in [
        (member, OrgRole::Member),
        (delegated, OrgRole::Admin),
        (asynchronous, OrgRole::Member),
    ] {
        applied(
            codeyard_db_orgs::organizations::membership::add_organization_member(
                f.pool(),
                owner,
                org.id,
                id,
                role,
            )
            .await
            .unwrap(),
        );
    }
    applied(
        codeyard_db_orgs::organizations::membership::add_organization_member(
            f.pool(),
            owner,
            foreign.id,
            member,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (id, name) in [
        (org.id, "team"),
        (org.id, "direct"),
        (org.id, "implicit"),
        (foreign.id, "other"),
    ] {
        let path = root
            .path()
            .join(format!("{id}-{name}.git"))
            .to_string_lossy()
            .into_owned();
        repos.push(
            codeyard_db_repos::repositories::creation::insert_repository(
                f.pool(),
                NewRepository {
                    owner: RepositoryCreationOwner {
                        actor_id: owner,
                        target: RepositoryTarget::Organization(id),
                    },
                    name,
                    description: None,
                    is_private: true,
                    disk_path: &path,
                    storage_node_id: None,
                    source_id: None,
                },
            )
            .await
            .unwrap()
            .unwrap(),
        );
    }
    let mut teams = Vec::new();
    for (slug, permission) in [("readers", 1), ("writers", 3)] {
        let team = applied(
            codeyard_db_orgs::organizations::teams::create_organization_team(
                f.pool(),
                owner,
                org.id,
                &NewTeam::new(
                    slug,
                    TeamProfile::new(slug, None, TeamPrivacy::Visible).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap(),
        );
        applied(
            codeyard_db_orgs::organizations::teams::grant_organization_team_repository(
                f.pool(),
                owner,
                org.id,
                team.id,
                repos[0].id,
                TeamRepositoryPermission::new(permission).unwrap(),
            )
            .await
            .unwrap(),
        );
        teams.push(team.id);
    }
    for (team, user) in [
        (teams[0], member),
        (teams[1], member),
        (teams[0], delegated),
        (teams[1], asynchronous),
    ] {
        applied(
            codeyard_db_orgs::organizations::teams::add_organization_team_member(
                f.pool(),
                owner,
                org.id,
                team,
                user,
                TeamRole::Member,
            )
            .await
            .unwrap(),
        );
    }
    for (user, repo, permission) in [
        (member, repos[0].id, AccessLevel::Read),
        (member, repos[1].id, AccessLevel::Maintain),
        (member, repos[3].id, AccessLevel::Write),
        (outsider, repos[0].id, AccessLevel::Read),
        (secure, repos[0].id, AccessLevel::Read),
    ] {
        codeyard_db_repos::repositories::access::grant_repo_access(
            f.pool(),
            user,
            repo,
            permission,
        )
        .await
        .unwrap();
    }
    let router = f.router(root.path()).await;
    let owner_browser = Client::login(&router, f.pool(), owner).await;
    let member_browser = Client::login(&router, f.pool(), member).await;
    let mut credentials = Vec::new();
    for (user, scope) in [
        (owner, "write"),
        (owner, "read"),
        (member, "write"),
        (delegated, "write"),
        (asynchronous, "write"),
        (outsider, "write"),
    ] {
        let browser = Client::login(&router, f.pool(), user).await;
        let token = codeyard_identity::tokens::create(
            f.pool(),
            user,
            Some(&browser.session),
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
    let delegated_token = Some(credentials[3].as_str());
    let path = "/api/v3/orgs/acme/outside_collaborators";
    for (token, status) in [
        (None, 401),
        (member_token, 403),
        (delegated_token, 403),
        (Some(credentials[5].as_str()), 403),
    ] {
        request(
            &router,
            "orgs/list-outside-collaborators",
            "GET",
            path,
            token,
            Value::Null,
            status,
        )
        .await;
    }
    for token in [
        read,
        member_token,
        delegated_token,
        Some(credentials[5].as_str()),
    ] {
        request(
            &router,
            "orgs/convert-member-to-outside-collaborator",
            "PUT",
            &format!("{path}/member"),
            token,
            json!({}),
            403,
        )
        .await;
        request(
            &router,
            "orgs/remove-outside-collaborator",
            "DELETE",
            &format!("{path}/outside"),
            token,
            Value::Null,
            403,
        )
        .await;
    }
    for asynchronous in [false, true] {
        request(
            &router,
            "orgs/convert-member-to-outside-collaborator",
            "PUT",
            &format!("{path}/owner"),
            write,
            json!({"async":asynchronous}),
            403,
        )
        .await;
    }
    request(
        &router,
        "orgs/convert-member-to-outside-collaborator",
        "PUT",
        &format!("{path}/outside"),
        write,
        json!({}),
        403,
    )
    .await;
    request(
        &router,
        "orgs/convert-member-to-outside-collaborator",
        "PUT",
        &format!("{path}/missing"),
        write,
        json!({}),
        404,
    )
    .await;
    let (_, error) = request(
        &router,
        "orgs/remove-outside-collaborator",
        "DELETE",
        &format!("{path}/member"),
        write,
        Value::Null,
        422,
    )
    .await;
    assert_eq!(
        error["message"],
        "You cannot specify an organization member to remove as an outside collaborator."
    );
    let (_, error) = request(
        &router,
        "orgs/list-outside-collaborators",
        "GET",
        &format!("{path}?filter=unknown"),
        read,
        Value::Null,
        422,
    )
    .await;
    assert_eq!(error["errors"][0]["field"], "filter");
    let (headers, body) = request(
        &router,
        "orgs/list-outside-collaborators",
        "GET",
        &format!("{path}?per_page=1"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], outsider);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let (_, body) = request(
        &router,
        "orgs/list-outside-collaborators",
        "GET",
        &format!("{path}?page=2&per_page=1"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], secure);
    f.execute(
        "UPDATE users SET two_factor_enabled=TRUE WHERE id=$1",
        &[secure],
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/list-outside-collaborators",
        "GET",
        &format!("{path}?filter=2fa_disabled"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], outsider);
    let (_, body) = request(
        &router,
        "orgs/list-outside-collaborators",
        "GET",
        &format!("{path}?filter=2fa_insecure"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body, json!([]));
    let settings = format!("/settings/organizations/{}?tab=members", org.id);
    let page = owner_browser.page_ok(&settings).await;
    assert!(page.contains("Outside collaborators"));
    assert!(page.contains("Convert to outside collaborator"));
    assert!(!member_browser
        .page_ok(&settings)
        .await
        .contains("Outside collaborators"));
    let form = format!("/settings/organizations/{}", org.id);
    let id = member.to_string();
    let (status, page) = owner_browser
        .form(
            &form,
            &[
                ("action", "convert-outside"),
                ("user_id", &id),
                ("confirm", "wrong"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM org_memberships WHERE org_id=$1 AND user_id=$2",
            &[org.id, member]
        )
        .await,
        1
    );
    let (status, page) = owner_browser
        .form(
            &form,
            &[
                ("action", "convert-outside"),
                ("user_id", &id),
                ("confirm", "member"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM org_memberships WHERE org_id=$1 AND user_id=$2",
            &[org.id, member]
        )
        .await,
        0
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM team_memberships WHERE user_id=$1",
            &[member]
        )
        .await,
        0
    );
    // Team permissions become explicit grants at their maximum. Existing stronger grants survive.
    assert_eq!(
        f.scalar(
            "SELECT access_level FROM repo_access WHERE repo_id=$1 AND user_id=$2",
            &[repos[0].id, member]
        )
        .await,
        3
    );
    assert_eq!(
        f.scalar(
            "SELECT access_level FROM repo_access WHERE repo_id=$1 AND user_id=$2",
            &[repos[1].id, member]
        )
        .await,
        4
    );
    let (_, body) = request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/team",
        member_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["permissions"]["push"], true);
    assert_eq!(body["permissions"]["admin"], false);
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/foreign/other",
        member_token,
        Value::Null,
        200,
    )
    .await;
    // Delegated administrative powers are not materialized as repository grants.
    let (_, body) = request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/implicit",
        delegated_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["permissions"]["admin"], true);
    request(
        &router,
        "orgs/convert-member-to-outside-collaborator",
        "PUT",
        &format!("{path}/delegated"),
        write,
        json!({}),
        204,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/implicit",
        delegated_token,
        Value::Null,
        404,
    )
    .await;
    let (_, body) = request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/team",
        delegated_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["permissions"]["pull"], true);
    assert_eq!(body["permissions"]["push"], false);
    assert_eq!(body["permissions"]["admin"], false);
    request(
        &router,
        "orgs/convert-member-to-outside-collaborator",
        "PUT",
        &format!("{path}/asynchronous"),
        write,
        json!({"async":true}),
        202,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if f.scalar(
                "SELECT COUNT(*) FROM org_memberships WHERE org_id=$1 AND user_id=$2",
                &[org.id, asynchronous],
            )
            .await
                == 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the accepted conversion job committed");
    assert_eq!(
        f.scalar(
            "SELECT access_level FROM repo_access WHERE repo_id=$1 AND user_id=$2",
            &[repos[0].id, asynchronous]
        )
        .await,
        3
    );
    request(
        &router,
        "orgs/remove-outside-collaborator",
        "DELETE",
        &format!("{path}/member"),
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/team",
        member_token,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/direct",
        member_token,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/foreign/other",
        member_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM org_memberships WHERE org_id=$1 AND user_id=$2",
            &[foreign.id, member]
        )
        .await,
        1
    );
    request(
        &router,
        "orgs/remove-outside-collaborator",
        "DELETE",
        &format!("{path}/member"),
        write,
        Value::Null,
        404,
    )
    .await;
    let id = outsider.to_string();
    let (status, page) = owner_browser
        .form(
            &form,
            &[
                ("action", "remove-outside"),
                ("user_id", &id),
                ("confirm", "outside"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repo_access WHERE repo_id=$1 AND user_id=$2",
            &[repos[0].id, outsider]
        )
        .await,
        0
    );
    f.finish().await;
}

#[tokio::test]
async fn github_outside_collaborators_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn github_outside_collaborators_postgres() {
    contract(true).await;
}

use super::request;
use crate::support::{database::Database, http_client::Client, organizations::applied};
use axum::http::StatusCode;
use codeyard::backend::db;
use codeyard::backend::db::organizations;
use codeyard::backend::db::organizations::NewOrganization;
use codeyard::backend::db::organizations::NewTeam;
use codeyard::backend::db::organizations::OrganizationProfile;
use codeyard::backend::db::organizations::TeamPrivacy;
use codeyard::backend::db::organizations::TeamProfile;
use codeyard::backend::db::organizations::TeamRole;
use codeyard::backend::db::repositories::creation::NewRepository;
use codeyard::backend::db::repositories::creation::RepositoryCreationOwner;
use codeyard::backend::db::repositories::creation::RepositoryTarget;
use codeyard::backend::identity::tokens;
use codeyard::backend::identity::tokens::TokenInput;
use codeyard_db_core::models::AccessLevel;
use codeyard_db_core::models::OrgRole;
use serde_json::{json, Value};

pub(super) async fn run(f: &Database, repo: i64, actor: i64, status: &str) {
    let sql="INSERT INTO ci_runs(run_id,repo_id,triggered_by_id,mode,pipeline,event,branch,workflow_path,status) VALUES($1,$2,$3,'local','ci','workflow_dispatch','main','.github/workflows/ci.yml',$4)";
    let id = uuid::Uuid::new_v4();
    match f.pool() {
        codeyard_db_core::DbPool::Sqlite(pool) => {
            sqlx::query(sql)
                .bind(id)
                .bind(repo)
                .bind(actor)
                .bind(status)
                .execute(pool)
                .await
                .unwrap();
        }
        codeyard_db_core::DbPool::Postgres(pool) => {
            sqlx::query(sql)
                .bind(id)
                .bind(repo)
                .bind(actor)
                .bind(status)
                .execute(pool)
                .await
                .unwrap();
        }
    }
}
pub(super) async fn repo(
    f: &Database,
    root: &std::path::Path,
    owner: i64,
    org: i64,
    name: &str,
) -> codeyard_db_core::models::Repository {
    let disk = root
        .join(format!("{}.git", uuid::Uuid::new_v4()))
        .to_string_lossy()
        .into_owned();
    let bare = git2::Repository::init_bare(&disk).unwrap();
    bare.set_head("refs/heads/main").unwrap();
    db::repositories::creation::insert_repository(
        f.pool(),
        NewRepository {
            owner: RepositoryCreationOwner {
                actor_id: owner,
                target: RepositoryTarget::Organization(org),
            },
            name,
            description: None,
            is_private: true,
            disk_path: &disk,
            storage_node_id: None,
            source_id: None,
        },
    )
    .await
    .unwrap()
    .unwrap()
}
async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let other = f.actor("other").await;
    let delegated = f.actor("delegated").await;
    let outsider = f.actor("outside").await;
    let mut orgs = Vec::new();
    for name in ["acme", "foreign"] {
        orgs.push(applied(
            organizations::create_organization(
                f.pool(),
                owner,
                &NewOrganization::new(name, OrganizationProfile::new(None, None).unwrap()).unwrap(),
            )
            .await
            .unwrap(),
        ));
    }
    for (id, role) in [
        (member, codeyard_db_core::models::OrgRole::Member),
        (other, codeyard_db_core::models::OrgRole::Member),
        (delegated, codeyard_db_core::models::OrgRole::Admin),
    ] {
        applied(
            organizations::add_organization_member(f.pool(), owner, orgs[0].id, id, role)
                .await
                .unwrap(),
        );
    }
    applied(
        organizations::add_organization_member(
            f.pool(),
            owner,
            orgs[1].id,
            member,
            codeyard_db_core::models::OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    let team = applied(
        organizations::create_organization_team(
            f.pool(),
            owner,
            orgs[0].id,
            &NewTeam::new(
                "builders",
                TeamProfile::new("Builders", None, TeamPrivacy::Secret).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        organizations::create_organization_team(
            f.pool(),
            owner,
            orgs[1].id,
            &NewTeam::new(
                "foreign-team",
                TeamProfile::new("Foreign", None, TeamPrivacy::Visible).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    for user in [member, other] {
        applied(
            organizations::add_organization_team_member(
                f.pool(),
                owner,
                orgs[0].id,
                team.id,
                user,
                TeamRole::Member,
            )
            .await
            .unwrap(),
        );
    }
    let root = tempfile::tempdir().unwrap();
    let first = repo(&f, root.path(), owner, orgs[0].id, "first").await;
    let foreign = repo(&f, root.path(), owner, orgs[1].id, "other").await;
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), owner).await;
    let member_browser = Client::login(&router, f.pool(), member).await;
    let other_browser = Client::login(&router, f.pool(), other).await;
    let mut credentials = Vec::new();
    for (id, scope) in [
        (owner, "write"),
        (owner, "read"),
        (member, "write"),
        (other, "write"),
        (delegated, "write"),
        (outsider, "write"),
    ] {
        let session = Client::login(&router, f.pool(), id).await;
        let token = tokens::create(
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
    let target = Some(credentials[2].as_str());
    let second = Some(credentials[3].as_str());
    let path = "/api/v3/orgs/acme/organization-roles";
    for (token, status) in [
        (None, 401),
        (target, 403),
        (Some(credentials[4].as_str()), 403),
        (Some(credentials[5].as_str()), 403),
    ] {
        request(
            &router,
            "orgs/list-org-roles",
            "GET",
            path,
            token,
            Value::Null,
            status,
        )
        .await;
        request(
            &router,
            "orgs/get-org-role",
            "GET",
            &format!("{path}/1"),
            token,
            Value::Null,
            status,
        )
        .await;
        request(
            &router,
            "orgs/list-org-role-users",
            "GET",
            &format!("{path}/1/users"),
            token,
            Value::Null,
            status,
        )
        .await;
        request(
            &router,
            "orgs/list-org-role-teams",
            "GET",
            &format!("{path}/1/teams"),
            token,
            Value::Null,
            status,
        )
        .await;
        request(
            &router,
            "orgs/assign-user-to-org-role",
            "PUT",
            &format!("{path}/users/member/1"),
            token,
            Value::Null,
            status,
        )
        .await;
        request(
            &router,
            "orgs/assign-team-to-org-role",
            "PUT",
            &format!("{path}/teams/builders/1"),
            token,
            Value::Null,
            status,
        )
        .await;
        request(
            &router,
            "orgs/revoke-all-org-roles-user",
            "DELETE",
            &format!("{path}/users/missing"),
            token,
            Value::Null,
            status,
        )
        .await;
        request(
            &router,
            "orgs/revoke-all-org-roles-team",
            "DELETE",
            &format!("{path}/teams/missing"),
            token,
            Value::Null,
            status,
        )
        .await;
    }
    request(
        &router,
        "orgs/assign-user-to-org-role",
        "PUT",
        &format!("{path}/users/member/1"),
        read,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "orgs/assign-team-to-org-role",
        "PUT",
        &format!("{path}/teams/builders/1"),
        read,
        Value::Null,
        403,
    )
    .await;
    let (_, catalog) = request(
        &router,
        "orgs/list-org-roles",
        "GET",
        path,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(catalog["total_count"], 6);
    for (index, base) in ["read", "triage", "write", "maintain", "admin"]
        .into_iter()
        .enumerate()
    {
        let role = &catalog["roles"][index];
        assert_eq!(role["base_role"], base);
        assert_eq!(role["source"], "Predefined");
        assert_eq!(role["organization"], Value::Null);
        let (_, single) = request(
            &router,
            "orgs/get-org-role",
            "GET",
            &format!("{path}/{}", role["id"]),
            read,
            Value::Null,
            200,
        )
        .await;
        assert_eq!(&single, role);
    }
    request(
        &router,
        "orgs/get-org-role",
        "GET",
        &format!("{path}/999"),
        read,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/assign-user-to-org-role",
        "PUT",
        &format!("{path}/users/outside/1"),
        write,
        Value::Null,
        422,
    )
    .await;
    request(
        &router,
        "orgs/assign-user-to-org-role",
        "PUT",
        &format!("{path}/users/member/999"),
        write,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/assign-team-to-org-role",
        "PUT",
        &format!("{path}/teams/foreign-team/3"),
        write,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/revoke-org-role-user",
        "DELETE",
        &format!("{path}/users/missing/999"),
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/revoke-org-role-team",
        "DELETE",
        &format!("{path}/teams/missing/999"),
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        target,
        Value::Null,
        404,
    )
    .await;
    // Usernames are exact: accounts may differ only in case, so MEMBER is not member.
    request(
        &router,
        "orgs/assign-user-to-org-role",
        "PUT",
        &format!("{path}/users/MEMBER/1"),
        write,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/assign-user-to-org-role",
        "PUT",
        &format!("{path}/users/member/1"),
        write,
        Value::Null,
        204,
    )
    .await;
    let (_, body) = request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        target,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["permissions"]["pull"], true);
    assert_eq!(body["permissions"]["push"], false);
    let metadata = applied(
        organizations::create_organization_team(
            f.pool(),
            owner,
            orgs[0].id,
            &NewTeam::new(
                "metadata",
                TeamProfile::new("Readable metadata", None, TeamPrivacy::Visible).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        organizations::grant_organization_team_repository(
            f.pool(),
            owner,
            orgs[0].id,
            metadata.id,
            first.id,
            organizations::TeamRepositoryPermission::new(1).unwrap(),
        )
        .await
        .unwrap(),
    );
    let (_, team_metadata) = request(
        &router,
        "teams/get-by-name",
        "GET",
        "/api/v3/orgs/acme/teams/metadata",
        target,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(team_metadata["repos_count"], 1);
    let (_, team_repos) = request(
        &router,
        "teams/list-repos-in-org",
        "GET",
        "/api/v3/orgs/acme/teams/metadata/repos",
        target,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(team_repos[0]["id"], first.id);
    for (operation, path) in [
        ("repos/list-for-authenticated-user", "/api/v3/user/repos"),
        ("repos/list-for-org", "/api/v3/orgs/acme/repos"),
    ] {
        let (_, listed) = request(&router, operation, "GET", path, target, Value::Null, 200).await;
        assert!(listed
            .as_array()
            .unwrap()
            .iter()
            .any(|repository| repository["id"] == first.id));
        assert!(listed
            .as_array()
            .unwrap()
            .iter()
            .all(|repository| repository["id"] != foreign.id));
    }
    assert_eq!(
        organizations::get_organization_repo_access(f.pool(), member, &first)
            .await
            .unwrap(),
        codeyard_db_core::models::AccessLevel::Read
    );
    request(
        &router,
        "issues/check-user-can-be-assigned",
        "GET",
        "/api/v3/repos/acme/first/assignees/member",
        read,
        Value::Null,
        204,
    )
    .await;
    assert_eq!(
        applied(
            organizations::get_organization_role(f.pool(), member, orgs[0].id)
                .await
                .unwrap()
        ),
        codeyard_db_core::models::OrgRole::Member
    );
    let (_, issue) = request(
        &router,
        "issues/create",
        "POST",
        "/api/v3/repos/acme/first/issues",
        write,
        json!({"title":"Role write authority"}),
        201,
    )
    .await;
    request(
        &router,
        "issues/create-label",
        "POST",
        "/api/v3/repos/acme/first/labels",
        write,
        json!({"name":"role-test","color":"00ff00"}),
        201,
    )
    .await;
    let labels = format!("/api/v3/repos/acme/first/issues/{}/labels", issue["number"]);
    request(
        &router,
        "issues/add-labels",
        "POST",
        &labels,
        target,
        json!({"labels":["role-test"]}),
        403,
    )
    .await;
    request(
        &router,
        "orgs/assign-team-to-org-role",
        "PUT",
        &format!("{path}/teams/builders/3"),
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "issues/add-labels",
        "POST",
        &labels,
        target,
        json!({"labels":["role-test"]}),
        200,
    )
    .await;
    let push_teams = db::repositories::push_teams::by_ids(f.pool(), first.id, &[team.id])
        .await
        .unwrap();
    assert_eq!(push_teams[0].permission, 3);
    assert!(
        db::repositories::push_teams::contains_actor(f.pool(), first.id, member, &[team.id])
            .await
            .unwrap()
    );
    let (_, users) = request(
        &router,
        "orgs/list-org-role-users",
        "GET",
        &format!("{path}/3/users"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(users.as_array().unwrap().len(), 2);
    assert_eq!(users[0]["assignment"], "indirect");
    assert_eq!(users[0]["inherited_from"][0]["id"], team.id);
    request(
        &router,
        "orgs/assign-user-to-org-role",
        "PUT",
        &format!("{path}/users/member/3"),
        write,
        Value::Null,
        204,
    )
    .await;
    let (headers, users) = request(
        &router,
        "orgs/list-org-role-users",
        "GET",
        &format!("{path}/3/users?per_page=1"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(users.as_array().unwrap().len(), 1);
    assert_eq!(users[0]["assignment"], "mixed");
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let (_, users) = request(
        &router,
        "orgs/list-org-role-users",
        "GET",
        &format!("{path}/3/users?per_page=1&page=2"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(users[0]["id"], other);
    let (_, teams) = request(
        &router,
        "orgs/list-org-role-teams",
        "GET",
        &format!("{path}/3/teams"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(teams[0]["id"], team.id);
    assert_eq!(teams[0]["assignment"], "direct");
    assert_eq!(teams[0]["permission"], "push");
    run(&f, first.id, member, "queued").await;
    run(&f, first.id, member, "running").await;
    run(&f, first.id, other, "queued").await;
    run(&f, foreign.id, member, "queued").await;
    request(
        &router,
        "orgs/revoke-org-role-user",
        "DELETE",
        &format!("{path}/users/member/3"),
        write,
        Value::Null,
        204,
    )
    .await;
    assert_eq!(f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='cancelled'",&[first.id,member]).await,1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='running' AND cancel_requested",&[first.id,member]).await,1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='queued'",&[first.id,other]).await,1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='queued'",&[foreign.id,member]).await,1);
    let (_, body) = request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        target,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["permissions"]["push"], true);
    request(
        &router,
        "orgs/revoke-all-org-roles-user",
        "DELETE",
        &format!("{path}/users/member"),
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/revoke-all-org-roles-team",
        "DELETE",
        &format!("{path}/teams/builders"),
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        target,
        Value::Null,
        404,
    )
    .await;
    for level in 1..=5 {
        request(
            &router,
            "orgs/assign-user-to-org-role",
            "PUT",
            &format!("{path}/users/member/{level}"),
            write,
            Value::Null,
            204,
        )
        .await;
        let (_, body) = request(
            &router,
            "repos/get",
            "GET",
            "/api/v3/repos/acme/first",
            target,
            Value::Null,
            200,
        )
        .await;
        for (permission, minimum) in [
            ("pull", 1),
            ("triage", 2),
            ("push", 3),
            ("maintain", 4),
            ("admin", 5),
        ] {
            assert_eq!(body["permissions"][permission], level >= minimum);
        }
        let (status, page) = member_browser.page("/acme/first/settings").await;
        assert_eq!(
            status,
            if level == 5 {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            },
            "{page}"
        );
        if level < 5 {
            request(
                &router,
                "orgs/revoke-org-role-user",
                "DELETE",
                &format!("{path}/users/member/{level}"),
                write,
                Value::Null,
                204,
            )
            .await;
        }
    }
    let future = repo(&f, root.path(), owner, orgs[0].id, "future").await;
    let (_, body) = request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/future",
        target,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["permissions"]["admin"], true);
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/foreign/other",
        target,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/list-org-roles",
        "GET",
        path,
        target,
        Value::Null,
        403,
    )
    .await;
    f.execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[owner])
        .await;
    let (_, body) = request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        target,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["permissions"]["push"], false);
    f.execute("UPDATE users SET is_banned=FALSE WHERE id=$1", &[owner])
        .await;
    f.execute(
        "UPDATE org_memberships SET role='unknown' WHERE org_id=$1 AND user_id=$2",
        &[orgs[0].id, member],
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        target,
        Value::Null,
        404,
    )
    .await;
    f.execute(
        "UPDATE org_memberships SET role='member' WHERE org_id=$1 AND user_id=$2",
        &[orgs[0].id, member],
    )
    .await;
    request(
        &router,
        "orgs/assign-user-to-org-role",
        "PUT",
        "/api/v3/orgs/foreign/organization-roles/users/member/1",
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/remove-member",
        "DELETE",
        "/api/v3/orgs/acme/members/member",
        write,
        Value::Null,
        204,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM organization_role_assignments WHERE org_id=$1 AND user_id=$2",
            &[orgs[0].id, member]
        )
        .await,
        0
    );
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        target,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/foreign/other",
        target,
        Value::Null,
        200,
    )
    .await;
    applied(
        organizations::add_organization_member(
            f.pool(),
            owner,
            orgs[0].id,
            member,
            codeyard_db_core::models::OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        organizations::add_organization_team_member(
            f.pool(),
            owner,
            orgs[0].id,
            team.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    request(
        &router,
        "orgs/assign-team-to-org-role",
        "PUT",
        &format!("{path}/teams/builders/5"),
        write,
        Value::Null,
        204,
    )
    .await;
    let (_, body) = request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        second,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["permissions"]["admin"], true);
    f.execute("UPDATE teams SET privacy='unknown' WHERE id=$1", &[team.id])
        .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        second,
        Value::Null,
        404,
    )
    .await;
    f.execute("UPDATE teams SET privacy='secret' WHERE id=$1", &[team.id])
        .await;
    request(
        &router,
        "orgs/convert-member-to-outside-collaborator",
        "PUT",
        "/api/v3/orgs/acme/outside_collaborators/member",
        write,
        json!({}),
        204,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT CAST(access_level AS BIGINT) FROM repo_access WHERE user_id=$1 AND repo_id=$2",
            &[member, first.id]
        )
        .await,
        5
    );
    assert_eq!(
        f.scalar(
            "SELECT CAST(access_level AS BIGINT) FROM repo_access WHERE user_id=$1 AND repo_id=$2",
            &[member, future.id]
        )
        .await,
        5
    );
    assert!(
        !db::repositories::push_teams::contains_actor(f.pool(), first.id, member, &[team.id])
            .await
            .unwrap()
    );
    repo(&f, root.path(), owner, orgs[0].id, "after-conversion").await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/after-conversion",
        target,
        Value::Null,
        404,
    )
    .await;
    let ui = format!("/settings/organizations/{}?tab=roles&role=5", orgs[0].id);
    let page = browser.page_ok(&ui).await;
    assert!(page.contains("All-repository admin"));
    assert!(page.contains("Assign to member"));
    assert!(!other_browser
        .page_ok(&ui)
        .await
        .contains("Assign to member"));
    let (status, page) = other_browser
        .form(
            &ui,
            &[
                ("action", "assign-user-role"),
                ("username", "other"),
                ("role_id", "5"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = browser
        .form(
            &ui,
            &[
                ("action", "assign-user-role"),
                ("username", "other"),
                ("role_id", "5"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let (status, page) = browser
        .form(
            &ui,
            &[
                ("action", "revoke-user-role"),
                ("username", "other"),
                ("role_id", "5"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM organization_role_assignments WHERE org_id=$1 AND user_id=$2",
            &[orgs[0].id, other]
        )
        .await,
        1
    );
    let (status, page) = browser
        .form(
            &ui,
            &[
                ("action", "revoke-all-user-roles"),
                ("username", "other"),
                ("confirm", "true"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let (_, body) = request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        second,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["permissions"]["admin"], true);
    // A revoke form for a team that no longer exists is refused, not reported as done.
    let (status, page) = browser
        .form(
            &ui,
            &[
                ("action", "revoke-all-team-roles"),
                ("team_id", "999999"),
                ("confirm", "true"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{page}");
    let (status, page) = browser
        .form(
            &ui,
            &[
                ("action", "revoke-all-team-roles"),
                ("team_id", &team.id.to_string()),
                ("confirm", "true"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        second,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        target,
        Value::Null,
        200,
    )
    .await;
    // An ancestor's organization role follows the shared nested-team authority, including
    // push restrictions, cleanup and conversion. It must never become org governance.
    let nested = f.actor("nested").await;
    applied(
        organizations::add_organization_member(
            f.pool(),
            owner,
            orgs[0].id,
            nested,
            codeyard_db_core::models::OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    let parent = applied(
        organizations::create_organization_team(
            f.pool(),
            owner,
            orgs[0].id,
            &NewTeam::new(
                "role-parent",
                TeamProfile::new("Role parent", None, TeamPrivacy::Visible).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let child = applied(
        organizations::create_organization_team(
            f.pool(),
            owner,
            orgs[0].id,
            &NewTeam::new(
                "role-child",
                TeamProfile::new("Role child", None, TeamPrivacy::Visible)
                    .unwrap()
                    .with_parent(Some(Some(parent.id)))
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        organizations::add_organization_team_member(
            f.pool(),
            owner,
            orgs[0].id,
            child.id,
            nested,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    let nested_browser = Client::login(&router, f.pool(), nested).await;
    let nested_token = tokens::create(
        f.pool(),
        nested,
        Some(&nested_browser.session),
        TokenInput {
            name: "nested",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let nested_bearer = format!("Bearer {}", nested_token.value);
    let nested_auth = Some(nested_bearer.as_str());
    request(
        &router,
        "orgs/assign-team-to-org-role",
        "PUT",
        &format!("{path}/teams/role-parent/3"),
        write,
        Value::Null,
        204,
    )
    .await;
    let (_, body) = request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        nested_auth,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["permissions"]["push"], true);
    assert_eq!(
        organizations::get_organization_repo_access(f.pool(), nested, &first)
            .await
            .unwrap(),
        codeyard_db_core::models::AccessLevel::Write
    );
    let (_, users) = request(
        &router,
        "orgs/list-org-role-users",
        "GET",
        &format!("{path}/3/users"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(users[0]["id"], nested);
    assert_eq!(users[0]["assignment"], "indirect");
    assert_eq!(users[0]["inherited_from"][0]["id"], parent.id);
    let (_, listed) = request(
        &router,
        "repos/list-for-authenticated-user",
        "GET",
        "/api/v3/user/repos",
        nested_auth,
        Value::Null,
        200,
    )
    .await;
    assert!(listed
        .as_array()
        .unwrap()
        .iter()
        .any(|repository| repository["id"] == first.id));
    assert_eq!(
        db::repositories::push_teams::by_ids(f.pool(), first.id, &[child.id])
            .await
            .unwrap()[0]
            .permission,
        3
    );
    assert!(db::repositories::push_teams::contains_actor(
        f.pool(),
        first.id,
        nested,
        &[parent.id, child.id]
    )
    .await
    .unwrap());
    request(
        &router,
        "issues/check-user-can-be-assigned",
        "GET",
        "/api/v3/repos/acme/first/assignees/nested",
        read,
        Value::Null,
        204,
    )
    .await;
    run(&f, first.id, nested, "queued").await;
    applied(
        organizations::delete_organization_team(f.pool(), owner, orgs[0].id, parent.id)
            .await
            .unwrap(),
    );
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        nested_auth,
        Value::Null,
        404,
    )
    .await;
    assert_eq!(f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='cancelled'",&[first.id,nested]).await,1);
    // The child team went with its parent; a new team of the same name stands in for it.
    assert!(
        organizations::get_public_team(f.pool(), owner, orgs[0].id, "role-child")
            .await
            .unwrap()
            .is_none()
    );
    let child = applied(
        organizations::create_organization_team(
            f.pool(),
            owner,
            orgs[0].id,
            &NewTeam::new(
                "role-child",
                TeamProfile::new("Role child", None, TeamPrivacy::Visible).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        organizations::add_organization_team_member(
            f.pool(),
            owner,
            orgs[0].id,
            child.id,
            nested,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    request(
        &router,
        "orgs/assign-team-to-org-role",
        "PUT",
        &format!("{path}/teams/role-child/3"),
        write,
        Value::Null,
        204,
    )
    .await;
    run(&f, first.id, nested, "queued").await;
    applied(
        organizations::remove_organization_team_member(
            f.pool(),
            owner,
            orgs[0].id,
            child.id,
            nested,
        )
        .await
        .unwrap(),
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='cancelled'",&[first.id,nested]).await,2);
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        nested_auth,
        Value::Null,
        404,
    )
    .await;
    applied(
        organizations::add_organization_team_member(
            f.pool(),
            owner,
            orgs[0].id,
            child.id,
            nested,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    request(
        &router,
        "orgs/convert-member-to-outside-collaborator",
        "PUT",
        "/api/v3/orgs/acme/outside_collaborators/nested",
        write,
        json!({}),
        204,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT CAST(access_level AS BIGINT) FROM repo_access WHERE user_id=$1 AND repo_id=$2",
            &[nested, first.id]
        )
        .await,
        3
    );
    // Ordinary team changes must not cancel unrelated repository jobs in the same org.
    let scoped = applied(
        organizations::create_organization_team(
            f.pool(),
            owner,
            orgs[0].id,
            &NewTeam::new(
                "scoped",
                TeamProfile::new("Scoped", None, TeamPrivacy::Visible).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        organizations::grant_organization_team_repository(
            f.pool(),
            owner,
            orgs[0].id,
            scoped.id,
            first.id,
            organizations::TeamRepositoryPermission::new(3).unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        organizations::add_organization_team_member(
            f.pool(),
            owner,
            orgs[0].id,
            scoped.id,
            other,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    f.execute(
        "INSERT INTO repo_access(user_id,repo_id,access_level) VALUES($1,$2,3)",
        &[other, future.id],
    )
    .await;
    let stopped=f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='cancelled'",&[first.id,other]).await;
    run(&f, first.id, other, "queued").await;
    run(&f, future.id, other, "queued").await;
    applied(
        organizations::remove_organization_team_member(
            f.pool(),
            owner,
            orgs[0].id,
            scoped.id,
            other,
        )
        .await
        .unwrap(),
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='cancelled'",&[first.id,other]).await,stopped+1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='queued'",&[future.id,other]).await,1);
    applied(
        organizations::add_organization_team_member(
            f.pool(),
            owner,
            orgs[0].id,
            scoped.id,
            other,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    run(&f, first.id, other, "queued").await;
    applied(
        organizations::delete_organization_team(f.pool(), owner, orgs[0].id, scoped.id)
            .await
            .unwrap(),
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='cancelled'",&[first.id,other]).await,stopped+2);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2 AND status='queued'",&[future.id,other]).await,1);
    f.finish().await;
}

#[tokio::test]
async fn github_organization_roles_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn github_organization_roles_postgres() {
    contract(true).await;
}

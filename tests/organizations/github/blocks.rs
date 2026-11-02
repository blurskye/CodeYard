use super::request;
use crate::support::{database::Database, http_client::Client, organizations::applied};
use axum::http::StatusCode;
use codeyard::backend::{
    db::{
        self,
        models::{AccessLevel, OrgRole},
        organizations::{self, NewOrganization, OrganizationProfile},
        repositories::creation::{NewRepository, RepositoryCreationOwner, RepositoryTarget},
    },
    identity::tokens::{self, TokenInput},
};
use serde_json::{json, Value};

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let admin = f.actor("delegated").await;
    let outsider = f.actor("outside").await;
    let another = f.actor("another").await;
    f.execute(
        "UPDATE users SET email='outside@example.test',email_verified=TRUE WHERE id=$1",
        &[outsider],
    )
    .await;
    let org = applied(
        organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let foreign = applied(
        organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("foreign", OrganizationProfile::new(None, None).unwrap())
                .unwrap(),
        )
        .await
        .unwrap(),
    );
    for (user, role) in [(member, OrgRole::Member), (admin, OrgRole::Admin)] {
        applied(
            organizations::add_organization_member(f.pool(), owner, org.id, user, role)
                .await
                .unwrap(),
        );
    }
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (id, name, private) in [
        (org.id, "public", false),
        (org.id, "private", true),
        (foreign.id, "public", false),
    ] {
        let path = root
            .path()
            .join(format!("{id}-{name}.git"))
            .to_string_lossy()
            .into_owned();
        repos.push(
            db::repositories::creation::insert_repository(
                f.pool(),
                NewRepository {
                    owner: RepositoryCreationOwner {
                        actor_id: owner,
                        target: RepositoryTarget::Organization(id),
                    },
                    name,
                    description: None,
                    is_private: private,
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
    for repo in &repos {
        db::repositories::access::grant_repo_access(
            f.pool(),
            outsider,
            repo.id,
            AccessLevel::Write,
        )
        .await
        .unwrap();
    }
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), owner).await;
    let member_browser = Client::login(&router, f.pool(), member).await;
    let mut credentials = Vec::new();
    for (user, scope) in [
        (owner, "write"),
        (owner, "read"),
        (member, "write"),
        (admin, "write"),
        (outsider, "write"),
    ] {
        let session = Client::login(&router, f.pool(), user).await;
        let token = tokens::create(
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
    let owner_token = Some(credentials[0].as_str());
    let read = Some(credentials[1].as_str());
    let target_token = Some(credentials[4].as_str());
    let path = "/api/v3/orgs/acme/blocks";
    for (token, status) in [
        (None, 401),
        (Some(credentials[2].as_str()), 403),
        (Some(credentials[3].as_str()), 403),
        (target_token, 403),
    ] {
        request(
            &router,
            "orgs/list-blocked-users",
            "GET",
            path,
            token,
            Value::Null,
            status,
        )
        .await;
        request(
            &router,
            "orgs/check-blocked-user",
            "GET",
            &format!("{path}/outside"),
            token,
            Value::Null,
            status,
        )
        .await;
        request(
            &router,
            "orgs/block-user",
            "PUT",
            &format!("{path}/outside"),
            token,
            Value::Null,
            status,
        )
        .await;
        request(
            &router,
            "orgs/unblock-user",
            "DELETE",
            &format!("{path}/outside"),
            token,
            Value::Null,
            status,
        )
        .await;
    }
    request(
        &router,
        "orgs/block-user",
        "PUT",
        &format!("{path}/outside"),
        read,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "orgs/block-user",
        "PUT",
        &format!("{path}/member"),
        owner_token,
        Value::Null,
        422,
    )
    .await;
    request(
        &router,
        "orgs/block-user",
        "PUT",
        &format!("{path}/owner"),
        owner_token,
        Value::Null,
        422,
    )
    .await;
    request(
        &router,
        "orgs/block-user",
        "PUT",
        &format!("{path}/missing"),
        owner_token,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/check-blocked-user",
        "GET",
        &format!("{path}/outside"),
        read,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "activity/star-repo-for-authenticated-user",
        "PUT",
        "/api/v3/user/starred/acme/public",
        target_token,
        Value::Null,
        204,
    )
    .await;
    let (_, issue) = request(
        &router,
        "issues/create",
        "POST",
        "/api/v3/repos/acme/public/issues",
        target_token,
        json!({"title":"Before blocking"}),
        201,
    )
    .await;
    let (_, owner_comment) = request(
        &router,
        "issues/create-comment",
        "POST",
        &format!(
            "/api/v3/repos/acme/public/issues/{}/comments",
            issue["number"]
        ),
        owner_token,
        json!({"body":"Owner reply"}),
        201,
    )
    .await;
    // With a mail server set up, the offer's email is queued in the transaction that saves it.
    f.execute(
        "INSERT INTO email_settings(id,host,port,security,from_address) VALUES(1,'mail.example.test',2525,'tls','noreply@example.test')",
        &[],
    )
    .await;
    let (_, invitation) = request(
        &router,
        "orgs/create-invitation",
        "POST",
        "/api/v3/orgs/acme/invitations",
        owner_token,
        json!({"invitee_id":outsider}),
        201,
    )
    .await;
    let invitation = invitation["id"].as_i64().unwrap();
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM email_outbox WHERE organization_invitation_id=$1 AND to_address='outside@example.test'",
            &[invitation]
        )
        .await,
        1
    );
    request(
        &router,
        "orgs/block-user",
        "PUT",
        &format!("{path}/outside"),
        owner_token,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/block-user",
        "PUT",
        &format!("{path}/outside"),
        owner_token,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/check-blocked-user",
        "GET",
        &format!("{path}/outside"),
        read,
        Value::Null,
        204,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM organization_invitations WHERE id=$1",
            &[invitation]
        )
        .await,
        0
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM email_outbox WHERE organization_invitation_id=$1",
            &[invitation]
        )
        .await,
        0
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repo_access WHERE repo_id=$1 AND user_id=$2",
            &[repos[1].id, outsider]
        )
        .await,
        0
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM stars WHERE repo_id=$1 AND user_id=$2",
            &[repos[0].id, outsider]
        )
        .await,
        0
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repo_access WHERE repo_id=$1 AND user_id=$2",
            &[repos[2].id, outsider]
        )
        .await,
        1
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM user_blocks WHERE user_id=$1 AND blocked_id=$2",
            &[owner, outsider]
        )
        .await,
        0
    );
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/private",
        target_token,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/public",
        target_token,
        Value::Null,
        200,
    )
    .await;
    request(
        &router,
        "issues/create",
        "POST",
        "/api/v3/repos/acme/public/issues",
        target_token,
        json!({"title":"Blocked"}),
        403,
    )
    .await;
    request(
        &router,
        "issues/create-comment",
        "POST",
        &format!(
            "/api/v3/repos/acme/public/issues/{}/comments",
            issue["number"]
        ),
        target_token,
        json!({"body":"Blocked"}),
        403,
    )
    .await;
    request(
        &router,
        "activity/star-repo-for-authenticated-user",
        "PUT",
        "/api/v3/user/starred/acme/public",
        target_token,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "issues/create",
        "POST",
        "/api/v3/repos/foreign/public/issues",
        target_token,
        json!({"title":"Still allowed"}),
        201,
    )
    .await;
    let metadata = json!({"comment_id":owner_comment["id"]});
    let notification = db::notifications::records::create_notification(
        f.pool(),
        db::notifications::NewNotification {
            user_id: outsider,
            actor_id: Some(owner),
            repo_id: Some(repos[0].id),
            kind: "issue_comment",
            title: "Owner reply",
            body: None,
            link: None,
            metadata: &metadata,
        },
    )
    .await;
    assert!(notification
        .err()
        .unwrap()
        .is::<db::notifications::records::NotificationSuppressed>());
    let fork = db::repositories::creation::insert_repository(
        f.pool(),
        NewRepository {
            owner: RepositoryCreationOwner::personal(outsider),
            name: "forbidden-fork",
            description: None,
            is_private: false,
            disk_path: "/unused/blocked-fork.git",
            storage_node_id: None,
            source_id: Some(repos[0].id),
        },
    )
    .await;
    assert_eq!(
        fork.err()
            .unwrap()
            .downcast_ref::<db::repositories::creation::RepositoryStorageRefusal>(),
        Some(&db::repositories::creation::RepositoryStorageRefusal::Denied)
    );
    request(
        &router,
        "orgs/create-invitation",
        "POST",
        "/api/v3/orgs/acme/invitations",
        owner_token,
        json!({"invitee_id":outsider}),
        403,
    )
    .await;
    request(
        &router,
        "orgs/create-invitation",
        "POST",
        "/api/v3/orgs/acme/invitations",
        owner_token,
        json!({"email":"outside@example.test"}),
        403,
    )
    .await;
    assert!(matches!(
        organizations::add_organization_member(f.pool(), owner, org.id, outsider, OrgRole::Member)
            .await
            .unwrap(),
        organizations::OrganizationOutcome::Denied
    ));
    // Even grants retained by a future repository transfer cannot override the organization block.
    f.execute(
        "INSERT INTO repo_access(user_id,repo_id,access_level) VALUES($1,$2,5)",
        &[outsider, repos[1].id],
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/private",
        target_token,
        Value::Null,
        404,
    )
    .await;
    f.execute(
        "DELETE FROM repo_access WHERE user_id=$1 AND repo_id=$2",
        &[outsider, repos[1].id],
    )
    .await;
    request(
        &router,
        "orgs/block-user",
        "PUT",
        &format!("{path}/another"),
        owner_token,
        Value::Null,
        204,
    )
    .await;
    let (headers, body) = request(
        &router,
        "orgs/list-blocked-users",
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
        "orgs/list-blocked-users",
        "GET",
        &format!("{path}?per_page=1&page=2"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], another);
    let (_, body) = request(
        &router,
        "orgs/list-blocked-users",
        "GET",
        "/api/v3/orgs/foreign/blocks",
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body, json!([]));
    let settings = format!("/settings/organizations/{}?tab=members", org.id);
    assert!(browser.page_ok(&settings).await.contains("Blocked users"));
    assert!(!member_browser
        .page_ok(&settings)
        .await
        .contains("Blocked users"));
    let form = format!("/settings/organizations/{}", org.id);
    let (status, page) = member_browser
        .form(&form, &[("action", "block-user"), ("username", "another")])
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let id = another.to_string();
    let (status, page) = browser
        .form(&form, &[("action", "unblock-user"), ("user_id", &id)])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    request(
        &router,
        "orgs/check-blocked-user",
        "GET",
        &format!("{path}/another"),
        read,
        Value::Null,
        404,
    )
    .await;
    let (status, page) = browser
        .form(&form, &[("action", "block-user"), ("username", "another")])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    request(
        &router,
        "orgs/check-blocked-user",
        "GET",
        &format!("{path}/another"),
        read,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/unblock-user",
        "DELETE",
        &format!("{path}/outside"),
        owner_token,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/unblock-user",
        "DELETE",
        &format!("{path}/outside"),
        owner_token,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/private",
        target_token,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "activity/star-repo-for-authenticated-user",
        "PUT",
        "/api/v3/user/starred/acme/public",
        target_token,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/create-invitation",
        "POST",
        "/api/v3/orgs/acme/invitations",
        owner_token,
        json!({"invitee_id":outsider}),
        201,
    )
    .await;
    request(
        &router,
        "orgs/update-membership-for-authenticated-user",
        "PATCH",
        "/api/v3/user/memberships/orgs/acme",
        target_token,
        json!({"state":"active"}),
        200,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn github_organization_blocks_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn github_organization_blocks_postgres() {
    contract(true).await;
}

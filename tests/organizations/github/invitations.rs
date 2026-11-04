use super::request;
use crate::support::{
    codeyard_db_orgs::organizations::applied, database::Database, http_client::Client,
};
use axum::http::StatusCode;

use codeyard_db_orgs::organizations::NewOrganization;
use codeyard_db_orgs::organizations::NewTeam;
use codeyard_db_orgs::organizations::OrganizationProfile;
use codeyard_db_orgs::organizations::TeamPrivacy;
use codeyard_db_orgs::organizations::TeamProfile;

use codeyard::backend::identity::tokens::TokenInput;
use codeyard_db_core::models::OrgRole;
use serde_json::{json, Value};

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let delegated = f.actor("delegated").await;
    let recipient = f.actor("recipient").await;
    let other = f.actor("other").await;
    f.execute(
        "UPDATE users SET email='other@example.com',email_verified=TRUE WHERE id=$1",
        &[other],
    )
    .await;
    let org = applied(
        codeyard_db_orgs::organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let foreign = applied(
        codeyard_db_orgs::organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("foreign", OrganizationProfile::new(None, None).unwrap())
                .unwrap(),
        )
        .await
        .unwrap(),
    );
    for (user, role) in [(member, OrgRole::Member), (delegated, OrgRole::Admin)] {
        applied(
            codeyard_db_orgs::organizations::add_organization_member(
                f.pool(),
                owner,
                org.id,
                user,
                role,
            )
            .await
            .unwrap(),
        );
    }
    let mut teams = Vec::new();
    for (org, slug) in [
        (org.id, "backend"),
        (org.id, "design"),
        (foreign.id, "foreign"),
    ] {
        teams.push(
            applied(
                codeyard_db_orgs::organizations::create_organization_team(
                    f.pool(),
                    owner,
                    org,
                    &NewTeam::new(
                        slug,
                        TeamProfile::new(slug, None, TeamPrivacy::Visible).unwrap(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap(),
            )
            .id,
        );
    }
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let owner_browser = Client::login(&router, f.pool(), owner).await;
    let recipient_browser = Client::login(&router, f.pool(), recipient).await;
    let mut credentials = Vec::new();
    for (user, scope) in [
        (owner, "write"),
        (owner, "read"),
        (member, "write"),
        (delegated, "write"),
        (recipient, "write"),
        (other, "write"),
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
    let invitee = Some(credentials[4].as_str());
    let other_token = Some(credentials[5].as_str());
    let path = "/api/v3/orgs/acme/invitations";
    let input =
        json!({"invitee_id":recipient,"role":"admin","team_ids":[teams[0],teams[1],teams[0]]});
    for (token, status) in [
        (None, 401),
        (read, 403),
        (Some(credentials[2].as_str()), 403),
        (Some(credentials[3].as_str()), 403),
        (invitee, 403),
    ] {
        request(
            &router,
            "orgs/create-invitation",
            "POST",
            path,
            token,
            input.clone(),
            status,
        )
        .await;
    }
    for input in [
        json!({}),
        json!({"email":"not an address"}),
        json!({"email":"one@example.com","invitee_id":recipient}),
        json!({"invitee_id":recipient,"role":"invented"}),
        json!({"invitee_id":recipient,"role":"billing_manager"}),
        json!({"invitee_id":recipient,"team_ids":[teams[2]]}),
        json!({"invitee_id":recipient,"team_ids":[999999]}),
        json!({"invitee_id":recipient,"role":"reinstate"}),
        json!({"invitee_id":owner}),
    ] {
        request(
            &router,
            "orgs/create-invitation",
            "POST",
            path,
            write,
            input,
            422,
        )
        .await;
    }
    let (_, body) = request(
        &router,
        "orgs/create-invitation",
        "POST",
        path,
        write,
        input.clone(),
        201,
    )
    .await;
    let first = body["id"].as_i64().unwrap();
    assert_eq!(body["login"], "recipient");
    assert_eq!(body["email"], Value::Null);
    assert_eq!(body["role"], "admin");
    assert_eq!(body["team_count"], 2);
    let (_, pending) = request(
        &router,
        "teams/list-pending-invitations-in-org",
        "GET",
        "/api/v3/orgs/acme/teams/backend/invitations",
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(pending[0]["id"], first);
    assert_eq!(pending[0]["team_count"], 2);
    let (_, membership) = request(
        &router,
        "teams/get-membership-for-user-in-org",
        "GET",
        "/api/v3/orgs/acme/teams/backend/memberships/recipient",
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(membership["state"], "pending");
    request(
        &router,
        "teams/add-or-update-membership-for-user-in-org",
        "PUT",
        "/api/v3/orgs/acme/teams/backend/memberships/recipient",
        Some(credentials[3].as_str()),
        json!({"role":"member"}),
        403,
    )
    .await;
    request(
        &router,
        "teams/add-or-update-membership-for-user-in-org",
        "PUT",
        "/api/v3/orgs/acme/teams/backend/memberships/recipient",
        write,
        json!({"role":"member"}),
        200,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM organization_invitations WHERE org_id=$1 AND user_id=$2",
            &[org.id, recipient]
        )
        .await,
        1
    );
    assert_eq!(body["inviter"]["login"], "owner");
    assert_eq!(body["invitation_source"], "member");
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM org_memberships WHERE org_id=$1 AND user_id=$2",
            &[org.id, recipient]
        )
        .await,
        0
    );
    request(
        &router,
        "orgs/create-invitation",
        "POST",
        path,
        write,
        input,
        422,
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/create-invitation",
        "POST",
        path,
        write,
        json!({"invitee_id":other}),
        201,
    )
    .await;
    let second = body["id"].as_i64().unwrap();
    assert_eq!(body["role"], "direct_member");
    let (headers, body) = request(
        &router,
        "orgs/list-pending-invitations",
        "GET",
        &format!("{path}?per_page=1"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], first);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let (_, body) = request(
        &router,
        "orgs/list-pending-invitations",
        "GET",
        &format!("{path}?per_page=1&page=2"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["id"], second);
    for (filter, expected) in [("role=admin", first), ("role=direct_member", second)] {
        let (_, body) = request(
            &router,
            "orgs/list-pending-invitations",
            "GET",
            &format!("{path}?{filter}"),
            read,
            Value::Null,
            200,
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["id"], expected);
    }
    for filter in [
        "invitation_source=scim",
        "role=billing_manager",
        "role=hiring_manager",
    ] {
        let (_, body) = request(
            &router,
            "orgs/list-pending-invitations",
            "GET",
            &format!("{path}?{filter}"),
            read,
            Value::Null,
            200,
        )
        .await;
        assert_eq!(body, json!([]));
    }
    let first_teams = format!("{path}/{first}/teams");
    for (operation, route) in [
        ("orgs/list-pending-invitations", path.to_owned()),
        (
            "orgs/list-failed-invitations",
            "/api/v3/orgs/acme/failed_invitations".into(),
        ),
        ("orgs/list-invitation-teams", first_teams.clone()),
    ] {
        request(&router, operation, "GET", &route, None, Value::Null, 401).await;
        for token in credentials.iter().skip(2) {
            request(
                &router,
                operation,
                "GET",
                &route,
                Some(token),
                Value::Null,
                403,
            )
            .await;
        }
    }
    let (headers, body) = request(
        &router,
        "orgs/list-invitation-teams",
        "GET",
        &format!("{first_teams}?per_page=1"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["id"], teams[0]);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let (_, body) = request(
        &router,
        "orgs/list-invitation-teams",
        "GET",
        &format!("{first_teams}?page=2&per_page=1"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["id"], teams[1]);
    request(
        &router,
        "orgs/list-invitation-teams",
        "GET",
        &format!("/api/v3/orgs/foreign/invitations/{first}/teams"),
        read,
        Value::Null,
        404,
    )
    .await;
    for token in [
        read,
        Some(credentials[2].as_str()),
        Some(credentials[3].as_str()),
        invitee,
    ] {
        request(
            &router,
            "orgs/cancel-invitation",
            "DELETE",
            &format!("{path}/{first}"),
            token,
            Value::Null,
            403,
        )
        .await;
    }
    request(
        &router,
        "orgs/cancel-invitation",
        "DELETE",
        &format!("/api/v3/orgs/foreign/invitations/{first}"),
        write,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/cancel-invitation",
        "DELETE",
        &format!("{path}/{second}"),
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/cancel-invitation",
        "DELETE",
        &format!("{path}/{second}"),
        write,
        Value::Null,
        404,
    )
    .await;
    // Acceptance adds organization and team membership together; reinstate uses the removed role.
    request(
        &router,
        "orgs/update-membership-for-authenticated-user",
        "PATCH",
        "/api/v3/user/memberships/orgs/acme",
        invitee,
        json!({"state":"active"}),
        200,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM team_memberships WHERE user_id=$1",
            &[recipient]
        )
        .await,
        2
    );
    request(
        &router,
        "orgs/remove-member",
        "DELETE",
        "/api/v3/orgs/acme/members/recipient",
        write,
        Value::Null,
        204,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM team_memberships WHERE user_id=$1",
            &[recipient]
        )
        .await,
        0
    );
    let (_, body) = request(
        &router,
        "orgs/create-invitation",
        "POST",
        path,
        write,
        json!({"invitee_id":recipient,"role":"reinstate"}),
        201,
    )
    .await;
    assert_eq!(body["role"], "admin");
    request(
        &router,
        "orgs/cancel-invitation",
        "DELETE",
        &format!("{path}/{}", body["id"]),
        write,
        Value::Null,
        204,
    )
    .await;
    // Email invitations may precede an account. Knowing an address is not proof of ownership.
    let (_, body) = request(
        &router,
        "orgs/create-invitation",
        "POST",
        path,
        write,
        json!({"email":"Late@Example.com", "team_ids":[teams[0],teams[1]]}),
        201,
    )
    .await;
    let email_invitation = body["id"].as_i64().unwrap();
    assert_eq!(body["login"], Value::Null);
    assert_eq!(body["email"], "late@example.com");
    let late = codeyard_db_identity::users::accounts::create_user(
        f.pool(),
        "late",
        Some("late@example.com"),
    )
    .await
    .unwrap()
    .id;
    let late_browser = Client::login(&router, f.pool(), late).await;
    let late_token = codeyard::backend::identity::tokens::create(
        f.pool(),
        late,
        Some(&late_browser.session),
        TokenInput {
            name: "late",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let late_token = format!("Bearer {}", late_token.value);
    request(
        &router,
        "orgs/get-membership-for-authenticated-user",
        "GET",
        "/api/v3/user/memberships/orgs/acme",
        Some(&late_token),
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/update-membership-for-authenticated-user",
        "PATCH",
        "/api/v3/user/memberships/orgs/acme",
        Some(&late_token),
        json!({"state":"active"}),
        404,
    )
    .await;
    request(
        &router,
        "teams/get-membership-for-user-in-org",
        "GET",
        "/api/v3/orgs/acme/teams/design/memberships/late",
        read,
        Value::Null,
        404,
    )
    .await;
    let code = "email-invitation-verification-code";
    assert!(codeyard_db_identity::email::save_email_token(
        f.pool(),
        &codeyard_db_identity::email::EmailLink {
            user_id: late,
            purpose: "verify",
            email: "late@example.com",
            token_hash: &codeyard::backend::identity::tokens::token_hash(code),
            ttl_seconds: 3600,
            subject: "Verify your email",
            body_text: "Verification code",
        },
    )
    .await
    .unwrap());
    let (status, page) = late_browser.form("/verify-email", &[("token", code)]).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let (_, pending) = request(
        &router,
        "teams/list-pending-invitations-in-org",
        "GET",
        "/api/v3/orgs/acme/teams/design/invitations",
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(pending[0]["id"], email_invitation);
    assert_eq!(pending[0]["email"], "late@example.com");
    assert_eq!(pending[0]["login"], Value::Null);
    request(
        &router,
        "teams/get-membership-for-user-in-org",
        "GET",
        "/api/v3/orgs/acme/teams/design/memberships/late",
        read,
        Value::Null,
        200,
    )
    .await;
    request(
        &router,
        "teams/add-or-update-membership-for-user-in-org",
        "PUT",
        "/api/v3/orgs/acme/teams/design/memberships/late",
        write,
        json!({"role":"maintainer"}),
        200,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM organization_invitations WHERE org_id=$1 AND user_id=$2",
            &[org.id, late]
        )
        .await,
        0
    );
    let (_, body) = request(
        &router,
        "orgs/get-membership-for-authenticated-user",
        "GET",
        "/api/v3/user/memberships/orgs/acme",
        Some(&late_token),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["state"], "pending");
    request(
        &router,
        "orgs/update-membership-for-authenticated-user",
        "PATCH",
        "/api/v3/user/memberships/orgs/acme",
        Some(&late_token),
        json!({"state":"active"}),
        200,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM team_memberships WHERE user_id=$1",
            &[late]
        )
        .await,
        2
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM team_memberships WHERE user_id=$1 AND team_id=$2 AND role='maintainer'",&[late,teams[1]]).await,1);
    request(
        &router,
        "orgs/list-invitation-teams",
        "GET",
        &format!("{path}/{email_invitation}/teams"),
        read,
        Value::Null,
        404,
    )
    .await;
    // Expiry is visible without mutating on GET/HEAD and an expired offer cannot grant access.
    let (_, body) = request(
        &router,
        "orgs/create-invitation",
        "POST",
        path,
        write,
        json!({"invitee_id":other,"team_ids":[teams[0]]}),
        201,
    )
    .await;
    let expired = body["id"].as_i64().unwrap();
    f.execute(
        "UPDATE organization_invitations SET expires_at='2000-01-01T00:00:00.000000Z' WHERE id=$1",
        &[expired],
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/list-failed-invitations",
        "GET",
        "/api/v3/orgs/acme/failed_invitations",
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["id"], expired);
    assert_eq!(body[0]["failed_reason"], "Invitation expired");
    assert!(body[0]["failed_at"].is_string());
    let page = owner_browser
        .page_ok(&format!("/settings/organizations/{}?tab=members", org.id))
        .await;
    assert!(page.contains("Failed invitations"));
    assert!(page.contains("Invitation expired"));
    request(
        &router,
        "orgs/update-membership-for-authenticated-user",
        "PATCH",
        "/api/v3/user/memberships/orgs/acme",
        other_token,
        json!({"state":"active"}),
        404,
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/create-invitation",
        "POST",
        path,
        write,
        json!({"invitee_id":other}),
        201,
    )
    .await;
    assert_eq!(body["id"], expired);
    assert_eq!(body["failed_at"], Value::Null);
    // Exhausting actual outbox attempts updates the linked invitation's failure state.
    // No mail server is set up, so the renewed offer queued no message; stand one in.
    f.execute(
        "INSERT INTO email_outbox(to_address,subject,body_text,organization_invitation_id) VALUES('other@example.com','Invitation','Accept in settings',$1)",
        &[expired],
    )
    .await;
    let mail = f
        .scalar(
            "SELECT id FROM email_outbox WHERE organization_invitation_id=$1",
            &[expired],
        )
        .await;
    f.execute("UPDATE email_outbox SET attempts=5 WHERE id=$1", &[mail])
        .await;
    codeyard_db_identity::email::mark_email_failed(
        f.pool(),
        mail,
        5,
        "SMTP rejected the recipient",
        60,
        5,
    )
    .await
    .unwrap();
    let (_, body) = request(
        &router,
        "orgs/list-failed-invitations",
        "GET",
        "/api/v3/orgs/acme/failed_invitations",
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(
        body[0]["failed_reason"],
        "Invitation email could not be delivered"
    );
    let (_, body) = request(
        &router,
        "orgs/list-pending-invitations",
        "GET",
        path,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body, json!([]));
    request(
        &router,
        "orgs/cancel-invitation",
        "DELETE",
        &format!("{path}/{expired}"),
        write,
        Value::Null,
        204,
    )
    .await;
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM email_outbox WHERE id=$1", &[mail])
            .await,
        0
    );
    // Native forms offer email recipients, named initial teams and restoration of a previous role.
    let form_path = format!("/settings/organizations/{}", org.id);
    let (status, page) = owner_browser
        .form(
            &form_path,
            &[
                ("action", "add-member"),
                ("username", "native@example.com"),
                ("role", "member"),
                ("team_slugs", "backend, design"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let (_, body) = request(
        &router,
        "orgs/list-pending-invitations",
        "GET",
        path,
        read,
        Value::Null,
        200,
    )
    .await;
    let native = body[0]["id"].as_i64().unwrap().to_string();
    assert_eq!(body[0]["email"], "native@example.com");
    assert_eq!(body[0]["team_count"], 2);
    let page = owner_browser
        .page_ok(&format!("{form_path}?tab=members"))
        .await;
    assert!(page.contains("native@example.com"));
    assert!(page.contains("Username or email address"));
    let (status, page) = owner_browser
        .form(
            &form_path,
            &[("action", "cancel-invitation"), ("invitation_id", &native)],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let (status, page) = owner_browser
        .form(
            &form_path,
            &[
                ("action", "add-member"),
                ("username", "recipient"),
                ("role", "member"),
                ("reinstate", "yes"),
                ("team_slugs", "backend"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let (_, body) = request(
        &router,
        "orgs/list-pending-invitations",
        "GET",
        path,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["role"], "admin");
    assert_eq!(body[0]["team_count"], 1);
    let (status, page) = recipient_browser
        .form(
            &format!("/settings/organizations/invitations/{}/accept", org.id),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM team_memberships WHERE user_id=$1",
            &[recipient]
        )
        .await,
        1
    );
    // Native team offers use the same expiry, retry, notification and initial-team relation.
    let renewal = f.actor("renewal").await;
    let backend = teams[0].to_string();
    let (status, page) = owner_browser
        .form(
            &form_path,
            &[
                ("action", "team-member"),
                ("team_id", &backend),
                ("username", "renewal"),
                ("role", "maintainer"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let offer = f
        .scalar(
            "SELECT id FROM organization_invitations WHERE org_id=$1 AND user_id=$2",
            &[org.id, renewal],
        )
        .await;
    assert_eq!(f.scalar("SELECT COUNT(*) FROM notifications WHERE user_id=$1 AND kind='organization_invitation'",&[renewal]).await,1);
    f.execute(
        "UPDATE organization_invitations SET expires_at='2000-01-01T00:00:00Z' WHERE id=$1",
        &[offer],
    )
    .await;
    request(
        &router,
        "teams/get-membership-for-user-in-org",
        "GET",
        "/api/v3/orgs/acme/teams/backend/memberships/renewal",
        read,
        Value::Null,
        404,
    )
    .await;
    let (_, pending) = request(
        &router,
        "teams/list-pending-invitations-in-org",
        "GET",
        "/api/v3/orgs/acme/teams/backend/invitations",
        read,
        Value::Null,
        200,
    )
    .await;
    assert!(pending
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["id"] != offer));
    request(
        &router,
        "teams/add-or-update-membership-for-user-in-org",
        "PUT",
        "/api/v3/orgs/acme/teams/design/memberships/renewal",
        write,
        json!({"role":"member"}),
        200,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT id FROM organization_invitations WHERE org_id=$1 AND user_id=$2",
            &[org.id, renewal]
        )
        .await,
        offer
    );
    let (_, initial) = request(
        &router,
        "orgs/list-invitation-teams",
        "GET",
        &format!("{path}/{offer}/teams"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(initial.as_array().unwrap().len(), 1);
    assert_eq!(initial[0]["id"], teams[1]);
    request(
        &router,
        "teams/get-membership-for-user-in-org",
        "GET",
        "/api/v3/orgs/acme/teams/backend/memberships/renewal",
        read,
        Value::Null,
        404,
    )
    .await;
    f.execute("UPDATE organization_invitations SET failed_at='2000-01-01T00:00:00Z',failed_reason='Invitation email delivery failed' WHERE id=$1",&[offer]).await;
    request(
        &router,
        "teams/get-membership-for-user-in-org",
        "GET",
        "/api/v3/orgs/acme/teams/design/memberships/renewal",
        read,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "teams/add-or-update-membership-for-user-in-org",
        "PUT",
        "/api/v3/orgs/acme/teams/design/memberships/renewal",
        write,
        json!({"role":"maintainer"}),
        200,
    )
    .await;
    let (_, membership) = request(
        &router,
        "teams/get-membership-for-user-in-org",
        "GET",
        "/api/v3/orgs/acme/teams/design/memberships/renewal",
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(membership["role"], "maintainer");
    let blocked = f.actor("blocked").await;
    request(
        &router,
        "orgs/block-user",
        "PUT",
        "/api/v3/orgs/acme/blocks/blocked",
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "teams/add-or-update-membership-for-user-in-org",
        "PUT",
        "/api/v3/orgs/acme/teams/backend/memberships/blocked",
        write,
        json!({"role":"member"}),
        403,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM organization_invitations WHERE org_id=$1 AND user_id=$2",
            &[org.id, blocked]
        )
        .await,
        0
    );
    f.finish().await;
}

#[tokio::test]
async fn github_organization_invitations_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn github_organization_invitations_postgres() {
    contract(true).await;
}

use super::github::request;
use crate::support::{
    database::{wait_until_blocked, Database},
    http_client::Client,
};
use codeyard::backend::db::{
    self,
    interactions::{
        InteractionExpiry, InteractionGroup, InteractionRefusal, InteractionScope, NewRestriction,
    },
    models::AccessLevel,
    users::accounts::{AccountCredential, AccountRestricted},
};
use codeyard::backend::identity::tokens::{self, TokenInput};
use serde_json::json;

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let carol = f.actor("carol").await;
    let dave = f.actor("dave").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), alice).await;
    let bob_session = Client::login(&router, f.pool(), bob).await;
    let dave_session = Client::login(&router, f.pool(), dave).await;
    let mut credentials = Vec::new();
    for (actor, session) in [(alice, &session.session), (bob, &bob_session.session)] {
        let token = tokens::create(
            f.pool(),
            actor,
            Some(session),
            TokenInput {
                name: "write",
                scope: "write",
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let write = Some(credentials[0].as_str());
    for (name, private) in [("project", false), ("unrelated", false), ("private", true)] {
        request(
            &router,
            "repos/create-for-authenticated-user",
            "POST",
            "/api/v3/user/repos",
            write,
            json!({"name":name,"private":private}),
            201,
        )
        .await;
    }
    let project = db::repositories::records::get_repository(f.pool(), "alice", "project")
        .await
        .unwrap()
        .unwrap();
    let private = db::repositories::records::get_repository(f.pool(), "alice", "private")
        .await
        .unwrap()
        .unwrap();
    db::repositories::access::grant_repo_access(f.pool(), bob, private.id, AccessLevel::Read)
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(f.pool(), bob, project.id, AccessLevel::Write)
        .await
        .unwrap();
    request(
        &router,
        "repos/create-or-update-file-contents",
        "PUT",
        "/api/v3/repos/alice/project/contents/contributed.txt",
        Some(&credentials[1]),
        json!({"message":"Prior contribution","content":"cHJpb3IK"}),
        201,
    )
    .await;
    db::repositories::access::revoke_repo_access(f.pool(), bob, project.id)
        .await
        .unwrap();
    request(
        &router,
        "interactions/get-restrictions-for-authenticated-user",
        "GET",
        "/api/v3/user/interaction-limits",
        write,
        json!(null),
        204,
    )
    .await;
    let (_, none) = request(
        &router,
        "interactions/get-restrictions-for-repo",
        "GET",
        "/api/v3/repos/alice/project/interaction-limits",
        write,
        json!(null),
        200,
    )
    .await;
    assert_eq!(none, json!({}));
    request(
        &router,
        "interactions/set-restrictions-for-authenticated-user",
        "PUT",
        "/api/v3/user/interaction-limits",
        write,
        json!({"limit":"invalid"}),
        422,
    )
    .await;
    let (_, restriction) = request(
        &router,
        "interactions/set-restrictions-for-repo",
        "PUT",
        "/api/v3/repos/alice/project/interaction-limits",
        write,
        json!({"limit":"collaborators_only","expiry":"one_day"}),
        200,
    )
    .await;
    assert_eq!(restriction["limit"], "collaborators_only");
    assert_eq!(restriction["origin"], "repository");
    let expires =
        chrono::DateTime::parse_from_rfc3339(restriction["expires_at"].as_str().unwrap()).unwrap();
    assert!((expires.timestamp() - chrono::Utc::now().timestamp() - 86400).abs() < 30);
    bob_session
        .expect(
            "POST",
            "/api/repos/alice/project/issues",
            json!({"title":"Refused"}),
            axum::http::StatusCode::FORBIDDEN,
        )
        .await;
    assert!(
        db::issues::create_issue(f.pool(), project.id, bob, "Refused", "Body")
            .await
            .is_err()
    );
    db::issues::create_issue(f.pool(), project.id, alice, "Owner allowed", "Body")
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(f.pool(), carol, project.id, AccessLevel::Read)
        .await
        .unwrap();
    assert!(db::issues::create_issue(
        f.pool(),
        project.id,
        carol,
        "Read is not collaboration",
        "Body"
    )
    .await
    .is_err());
    db::repositories::access::grant_repo_access(f.pool(), carol, project.id, AccessLevel::Write)
        .await
        .unwrap();
    db::issues::create_issue(f.pool(), project.id, carol, "Collaborator allowed", "Body")
        .await
        .unwrap();
    request(
        &router,
        "interactions/set-restrictions-for-repo",
        "PUT",
        "/api/v3/repos/alice/project/interaction-limits",
        write,
        json!({"limit":"contributors_only"}),
        200,
    )
    .await;
    // Real Git authorship on the immutable default-branch history, not issue/comment authorship.
    bob_session
        .expect(
            "POST",
            "/api/repos/alice/project/issues",
            json!({"title":"Prior contributor allowed"}),
            axum::http::StatusCode::CREATED,
        )
        .await;
    assert_eq!(f.scalar("SELECT COUNT(*) FROM interaction_contributors WHERE repo_id=$1 AND user_id=$2 AND contributed",&[project.id,bob]).await,1);
    dave_session
        .expect(
            "POST",
            "/api/repos/alice/project/issues",
            json!({"title":"Not a contributor"}),
            axum::http::StatusCode::FORBIDDEN,
        )
        .await;
    // A publication, default-branch change, placement change or email change invalidates the proof.
    request(
        &router,
        "repos/create-or-update-file-contents",
        "PUT",
        "/api/v3/repos/alice/project/contents/version.txt",
        write,
        json!({"message":"Advance the source","content":"bmV3Cg=="}),
        201,
    )
    .await;
    assert!(
        db::issues::create_issue(f.pool(), project.id, bob, "Stale proof", "Body")
            .await
            .is_err()
    );
    bob_session
        .expect(
            "POST",
            "/api/repos/alice/project/issues",
            json!({"title":"Refreshed contributor proof"}),
            axum::http::StatusCode::CREATED,
        )
        .await;
    request(
        &router,
        "interactions/set-restrictions-for-repo",
        "PUT",
        "/api/v3/repos/alice/project/interaction-limits",
        write,
        json!({"limit":"existing_users","expiry":"three_days"}),
        200,
    )
    .await;
    dave_session
        .expect(
            "POST",
            "/api/repos/alice/project/issues",
            json!({"title":"New account refused"}),
            axum::http::StatusCode::FORBIDDEN,
        )
        .await;
    bob_session
        .expect(
            "POST",
            "/api/repos/alice/project/issues",
            json!({"title":"New prior contributor allowed"}),
            axum::http::StatusCode::CREATED,
        )
        .await;
    f.execute(
        "UPDATE users SET created_at='2020-01-01T00:00:00.000000Z' WHERE id=$1",
        &[dave],
    )
    .await;
    dave_session
        .expect(
            "POST",
            "/api/repos/alice/project/issues",
            json!({"title":"Existing account allowed"}),
            axum::http::StatusCode::CREATED,
        )
        .await;
    let (_, owner_limit) = request(
        &router,
        "interactions/set-restrictions-for-authenticated-user",
        "PUT",
        "/api/v3/user/interaction-limits",
        write,
        json!({"limit":"collaborators_only","expiry":"one_month"}),
        200,
    )
    .await;
    assert_eq!(owner_limit["origin"], "user");
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM interaction_limits WHERE repo_id=$1",
            &[project.id]
        )
        .await,
        0
    );
    let (_, inherited) = request(
        &router,
        "interactions/get-restrictions-for-repo",
        "GET",
        "/api/v3/repos/alice/project/interaction-limits",
        write,
        json!(null),
        200,
    )
    .await;
    assert_eq!(inherited, owner_limit);
    request(
        &router,
        "interactions/get-restrictions-for-authenticated-user",
        "GET",
        "/api/v3/user/interaction-limits",
        write,
        json!(null),
        200,
    )
    .await;
    request(
        &router,
        "interactions/set-restrictions-for-repo",
        "PUT",
        "/api/v3/repos/alice/project/interaction-limits",
        write,
        json!({"limit":"existing_users"}),
        409,
    )
    .await;
    request(
        &router,
        "interactions/remove-restrictions-for-repo",
        "DELETE",
        "/api/v3/repos/alice/project/interaction-limits",
        write,
        json!(null),
        409,
    )
    .await;
    dave_session
        .expect(
            "POST",
            "/api/repos/alice/unrelated/issues",
            json!({"title":"Owner scope refused"}),
            axum::http::StatusCode::FORBIDDEN,
        )
        .await;
    bob_session
        .expect(
            "POST",
            "/api/repos/alice/private/issues",
            json!({"title":"Private repository unaffected"}),
            axum::http::StatusCode::CREATED,
        )
        .await;
    let page = session.page_ok("/alice/project/settings").await;
    assert!(
        page.contains("managed by the repository&#39;s owner")
            || page.contains("managed by the repository&#x27;s owner")
            || page.contains("managed by the repository's owner"),
        "{page}"
    );
    let page = session.page_ok("/settings?tab=blocked").await;
    assert!(page.contains("/settings/interaction-limits"));
    let reply = session
        .form_reply(
            "/settings/interaction-limits",
            &[
                ("scope", "user"),
                ("scope_id", &alice.to_string()),
                ("limit", "existing_users"),
                ("expiry", "one_week"),
            ],
        )
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::SEE_OTHER);
    request(
        &router,
        "interactions/remove-restrictions-for-authenticated-user",
        "DELETE",
        "/api/v3/user/interaction-limits",
        write,
        json!(null),
        204,
    )
    .await;
    request(
        &router,
        "interactions/get-restrictions-for-authenticated-user",
        "GET",
        "/api/v3/user/interaction-limits",
        write,
        json!(null),
        204,
    )
    .await;
    let organization = db::organizations::profiles::NewOrganization::new(
        "acme",
        db::organizations::profiles::OrganizationProfile::new(None, None).unwrap(),
    )
    .unwrap();
    let organization =
        match db::organizations::profiles::create_organization(f.pool(), alice, &organization)
            .await
            .unwrap()
        {
            db::organizations::OrganizationOutcome::Applied(org) => org,
            _ => panic!("organization creation refused"),
        };
    session
        .expect(
            "POST",
            "/api/repos",
            json!({"name":"shared","owner_org_id":organization.id}),
            axum::http::StatusCode::CREATED,
        )
        .await;
    let (_, none) = request(
        &router,
        "interactions/get-restrictions-for-org",
        "GET",
        "/api/v3/orgs/acme/interaction-limits",
        write,
        json!(null),
        200,
    )
    .await;
    assert_eq!(none, json!({}));
    let (_, restriction) = request(
        &router,
        "interactions/set-restrictions-for-org",
        "PUT",
        "/api/v3/orgs/acme/interaction-limits",
        write,
        json!({"limit":"collaborators_only","expiry":"six_months"}),
        200,
    )
    .await;
    assert_eq!(restriction["origin"], "organization");
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'member')",
        &[organization.id, bob],
    )
    .await;
    bob_session
        .expect(
            "POST",
            "/api/repos/acme/shared/issues",
            json!({"title":"Organization member allowed"}),
            axum::http::StatusCode::CREATED,
        )
        .await;
    dave_session
        .expect(
            "POST",
            "/api/repos/acme/shared/issues",
            json!({"title":"Nonmember refused"}),
            axum::http::StatusCode::FORBIDDEN,
        )
        .await;
    f.execute(
        "UPDATE org_memberships SET role='admin' WHERE org_id=$1 AND user_id=$2",
        &[organization.id, bob],
    )
    .await;
    request(
        &router,
        "interactions/get-restrictions-for-org",
        "GET",
        "/api/v3/orgs/acme/interaction-limits",
        Some(&credentials[1]),
        json!(null),
        200,
    )
    .await;
    let error = db::interactions::set_restriction(
        f.pool(),
        bob,
        &AccountCredential::Session(&bob_session.session),
        InteractionScope::Organization(organization.id),
        Some(NewRestriction {
            limit: InteractionGroup::ExistingUsers,
            expiry: InteractionExpiry::OneDay,
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<InteractionRefusal>(),
        Some(InteractionRefusal::Denied)
    ));
    let page = session
        .page_ok(&format!("/settings/organizations/{}", organization.id))
        .await;
    assert!(page.contains("name=\"scope\" value=\"organization\""));
    let reply = session
        .form_reply(
            "/settings/interaction-limits",
            &[
                ("scope", "organization"),
                ("scope_id", &organization.id.to_string()),
                ("remove", "yes"),
            ],
        )
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::SEE_OTHER);
    request(
        &router,
        "interactions/remove-restrictions-for-org",
        "DELETE",
        "/api/v3/orgs/acme/interaction-limits",
        write,
        json!(null),
        204,
    )
    .await;
    let (_, none) = request(
        &router,
        "interactions/get-restrictions-for-org",
        "GET",
        "/api/v3/orgs/acme/interaction-limits",
        write,
        json!(null),
        200,
    )
    .await;
    assert_eq!(none, json!({}));
    let reply = session
        .form_reply(
            "/settings/interaction-limits",
            &[
                ("scope", "repository"),
                ("scope_id", &project.id.to_string()),
                ("limit", "collaborators_only"),
                ("expiry", "one_day"),
            ],
        )
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::SEE_OTHER);
    let reply = bob_session
        .form_reply(
            "/settings/interaction-limits",
            &[
                ("scope", "user"),
                ("scope_id", &alice.to_string()),
                ("remove", "yes"),
            ],
        )
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::FORBIDDEN);
    f.execute(
        "UPDATE interaction_limits SET expires_at='2020-01-01T00:00:00.000000Z' WHERE repo_id=$1",
        &[project.id],
    )
    .await;
    let (_, none) = request(
        &router,
        "interactions/get-restrictions-for-repo",
        "GET",
        "/api/v3/repos/alice/project/interaction-limits",
        write,
        json!(null),
        200,
    )
    .await;
    assert_eq!(none, json!({}));
    dave_session
        .expect(
            "POST",
            "/api/repos/alice/project/issues",
            json!({"title":"Expired limit no longer applies"}),
            axum::http::StatusCode::CREATED,
        )
        .await;
    request(
        &router,
        "interactions/remove-restrictions-for-repo",
        "DELETE",
        "/api/v3/repos/alice/project/interaction-limits",
        write,
        json!(null),
        204,
    )
    .await;
    let error = db::interactions::set_restriction(
        f.pool(),
        alice,
        &AccountCredential::Token(i64::MAX),
        InteractionScope::User(alice),
        Some(NewRestriction {
            limit: InteractionGroup::CollaboratorsOnly,
            expiry: InteractionExpiry::OneDay,
        }),
    )
    .await
    .unwrap_err();
    assert!(error.is::<AccountRestricted>());
    if pg {
        let mut gate = f.pg().begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate)
            .await
            .unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_schema()),37002)")
            .execute(&mut *gate)
            .await
            .unwrap();
        sqlx::query("INSERT INTO interaction_limits(repo_id,restriction,expires_at) VALUES($1,'collaborators_only',clock_timestamp()+INTERVAL '1 day')").bind(project.id).execute(&mut *gate).await.unwrap();
        let pool = f.pool().clone();
        let repo = project.id;
        let writer = tokio::spawn(async move {
            db::issues::create_issue(&pool, repo, bob, "Pending policy", "Body")
                .await
                .is_err()
        });
        wait_until_blocked(f.pg(), pid, 1).await;
        gate.commit().await.unwrap();
        assert!(writer.await.unwrap());
    }
    f.finish().await;
}
#[tokio::test]
async fn sqlite_github_interaction_limits() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_interaction_limits() {
    contract(true).await;
}

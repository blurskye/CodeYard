//! Organization-owned fine-grained tokens: the owners' policy, approval, revocation and
//! membership, through the REST grant endpoints and the settings page on both engines.
use super::request;
use crate::support::{database::Database, http_client::Client, organizations::applied};
use chrono::{Duration, Utc};
use codeyard::{
    backend::{
        db::{
            self,
            models::{AccessLevel, OrgRole},
            organizations::{self, NewOrganization, OrganizationProfile},
            repositories::creation::{RepositoryCreationOwner, RepositoryTarget},
        },
        identity::{
            fine_grained::{self, Access, Owner, TokenError},
            tokens::{self, TokenInput},
        },
        repositories::lifecycle,
    },
    webui::app::Page,
};
use serde_json::{json, Value};

async fn classic(pool: &db::DbPool, user: i64, session: &str) -> String {
    let token = tokens::create(
        pool,
        user,
        Some(session),
        TokenInput {
            name: "classic",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    format!("Bearer {}", token.value)
}

async fn fine(
    pool: &db::DbPool,
    user: i64,
    session: &str,
    org: i64,
    days: i64,
) -> Result<(i64, String, String), TokenError> {
    fine_grained::create(
        pool,
        user,
        Some(session),
        fine_grained::Request {
            name: format!("token-{user}-{days}"),
            description: "Release automation".into(),
            owner: Owner::Organization(org),
            access: Access::All,
            repositories: vec![],
            permissions: json!({"issues":"write"}).as_object().unwrap().clone(),
            expires_at: Some(Utc::now() + Duration::days(days)),
        },
    )
    .await
    .map(|created| {
        (
            created.id,
            format!("Bearer {}", created.value),
            created.status,
        )
    })
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let carol = f.actor("carol").await;
    let dave = f.actor("dave").await;
    let org = applied(
        organizations::create_organization(
            f.pool(),
            alice,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    for user in [carol, dave] {
        applied(
            organizations::add_organization_member(f.pool(), alice, org, user, OrgRole::Member)
                .await
                .unwrap(),
        );
    }
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (name, private) in [("internal", true), ("open", false), ("secret", true)] {
        repos.push(
            lifecycle::provision(
                (f.pool(), None),
                root.path(),
                RepositoryCreationOwner {
                    actor_id: alice,
                    target: RepositoryTarget::Organization(org),
                },
                name,
                None,
                private,
                None,
            )
            .await
            .unwrap(),
        );
    }
    for user in [carol, dave] {
        db::repositories::access::grant_repo_access(f.pool(), user, repos[0].id, AccessLevel::Read)
            .await
            .unwrap();
    }
    let router = f.router(root.path()).await;
    let owner = Client::login(&router, f.pool(), alice).await;
    let member = Client::login(&router, f.pool(), carol).await;
    let other = Client::login(&router, f.pool(), dave).await;
    let admin = classic(f.pool(), alice, &owner.session).await;
    let settings = format!("/settings/organizations/{org}/personal-access-tokens");
    let tab = format!("/settings/organizations/{org}?tab=personal-access-tokens");

    // Owners set the policy on the settings page; members cannot.
    let policy = [
        ("action", "token-policy"),
        ("fine_grained_allowed", "true"),
        ("fine_grained_approval", "true"),
        ("classic_allowed", "true"),
        ("max_lifetime_days", "30"),
    ];
    let reply = member.form_reply(&settings, &policy).await;
    assert_eq!(reply.status, 403, "{}", reply.text());
    assert_eq!(
        member.page(&tab).await.0,
        200,
        "members see the overview instead"
    );
    let reply = owner.form_reply(&settings, &policy).await;
    assert_eq!(reply.status, 303, "{}", reply.text());
    assert_eq!(reply.location.as_deref(), Some(tab.as_str()));
    let saved = db::fine_grained_tokens::policy(f.pool(), org)
        .await
        .unwrap();
    assert_eq!(saved.max_lifetime_days, Some(30));
    let reply = owner
        .form_reply(
            &settings,
            &[("action", "token-policy"), ("max_lifetime_days", "999")],
        )
        .await;
    assert_eq!(reply.status, 400, "{}", reply.text());
    assert!(reply.text().contains("999"), "the refused value is kept");

    // A member's token waits for approval within the maximum lifetime, and meanwhile reads
    // public data only.
    assert!(matches!(
        fine(f.pool(), carol, &member.session, org, 60).await,
        Err(TokenError::Invalid(_))
    ));
    let (pending, carol_token, status) = fine(f.pool(), carol, &member.session, org, 30)
        .await
        .unwrap();
    assert_eq!(status, "pending");
    let issues = |repo: &str| format!("/api/v3/repos/acme/{repo}/issues");
    for (repo, expected) in [("internal", 404), ("open", 200)] {
        request(
            &router,
            "issues/list-for-repo",
            "GET",
            &issues(repo),
            Some(&carol_token),
            Value::Null,
            expected,
        )
        .await;
    }
    // An owner's own token needs no approval.
    let (_, owned, status) = fine(f.pool(), alice, &owner.session, org, 30)
        .await
        .unwrap();
    assert_eq!(status, "approved");
    request(
        &router,
        "issues/list-for-repo",
        "GET",
        &issues("secret"),
        Some(&owned),
        Value::Null,
        200,
    )
    .await;

    // Owners list requests; members and fine-grained tokens cannot.
    let requests = "/api/v3/orgs/acme/personal-access-token-requests";
    let (_, listed) = request(
        &router,
        "orgs/list-pat-grant-requests",
        "GET",
        requests,
        Some(&admin),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    let first = &listed[0];
    assert_eq!(first["id"], pending);
    assert_eq!(first["token_id"], pending);
    assert_eq!(first["owner"]["login"], "carol");
    assert_eq!(first["reason"], "Release automation");
    assert_eq!(first["repository_selection"], "all");
    assert_eq!(
        first["permissions"]["repository"],
        json!({"issues":"write","metadata":"read"})
    );
    assert_eq!(first["token_expired"], false);
    for (token, expected) in [(&carol_token, 403), (&owned, 403)] {
        request(
            &router,
            "orgs/list-pat-grant-requests",
            "GET",
            requests,
            Some(token),
            Value::Null,
            expected,
        )
        .await;
    }
    let (_, filtered) = request(
        &router,
        "orgs/list-pat-grant-requests",
        "GET",
        &format!("{requests}?owner=dave"),
        Some(&admin),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(filtered, json!([]));
    // A token with all repositories lists the ones its user can read.
    let (_, reachable) = request(
        &router,
        "orgs/list-pat-grant-request-repositories",
        "GET",
        &format!("{requests}/{pending}/repositories"),
        Some(&admin),
        Value::Null,
        200,
    )
    .await;
    let mut names: Vec<_> = reachable
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["internal", "open"]);
    // The settings page shows the request to owners.
    let (_, page) = owner.model(&tab).await;
    let Page::Organizations(page) = page else {
        panic!("organization settings")
    };
    let panel = page.tokens.expect("the token tab");
    assert_eq!(panel.max_lifetime_days, "30");
    assert_eq!(panel.pending.len(), 1);
    assert_eq!(panel.pending[0].user, "carol");
    assert!(panel.pending[0]
        .permissions
        .contains(&"Issues: write".to_owned()));

    // Approval reaches private repositories within the user's own rights.
    request(
        &router,
        "orgs/review-pat-grant-request",
        "POST",
        &format!("{requests}/{pending}"),
        Some(&admin),
        json!({"action":"approve"}),
        204,
    )
    .await;
    request(
        &router,
        "orgs/review-pat-grant-request",
        "POST",
        &format!("{requests}/{pending}"),
        Some(&admin),
        json!({"action":"approve"}),
        404,
    )
    .await;
    request(
        &router,
        "issues/list-for-repo",
        "GET",
        &issues("internal"),
        Some(&carol_token),
        Value::Null,
        200,
    )
    .await;
    request(
        &router,
        "issues/list-for-repo",
        "GET",
        &issues("secret"),
        Some(&carol_token),
        Value::Null,
        404,
    )
    .await;
    let (_, label) = request(
        &router,
        "issues/create-label",
        "POST",
        "/api/v3/repos/acme/internal/labels",
        Some(&carol_token),
        json!({"name":"bug","color":"ff0000"}),
        403,
    )
    .await;
    assert!(label["message"].is_string());
    let grants = "/api/v3/orgs/acme/personal-access-tokens";
    let (_, active) = request(
        &router,
        "orgs/list-pat-grants",
        "GET",
        grants,
        Some(&admin),
        Value::Null,
        200,
    )
    .await;
    let ids: Vec<_> = active
        .as_array()
        .unwrap()
        .iter()
        .map(|grant| grant["id"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&pending), "{active}");
    assert!(active[0]["access_granted_at"].is_string());

    // Revoking stops private access at once.
    request(
        &router,
        "orgs/update-pat-access",
        "POST",
        &format!("{grants}/{pending}"),
        Some(&admin),
        json!({"action":"revoke"}),
        204,
    )
    .await;
    request(
        &router,
        "issues/list-for-repo",
        "GET",
        &issues("internal"),
        Some(&carol_token),
        Value::Null,
        404,
    )
    .await;

    // Owners review on the settings page too, and leaving the organization ends access.
    let (second, second_token, _) = fine(f.pool(), carol, &member.session, org, 7)
        .await
        .unwrap();
    let id = second.to_string();
    let reply = member
        .form_reply(
            &settings,
            &[
                ("action", "token-review"),
                ("token_id", &id),
                ("decision", "approve"),
            ],
        )
        .await;
    assert_eq!(reply.status, 403, "{}", reply.text());
    let reply = owner
        .form_reply(
            &settings,
            &[
                ("action", "token-review"),
                ("token_id", &id),
                ("decision", "approve"),
                ("reason", "ok"),
            ],
        )
        .await;
    assert_eq!(reply.status, 303, "{}", reply.text());
    request(
        &router,
        "issues/list-for-repo",
        "GET",
        &issues("internal"),
        Some(&second_token),
        Value::Null,
        200,
    )
    .await;
    applied(
        organizations::remove_organization_member(f.pool(), alice, org, carol)
            .await
            .unwrap(),
    );
    request(
        &router,
        "issues/list-for-repo",
        "GET",
        &issues("internal"),
        Some(&second_token),
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "issues/list-for-repo",
        "GET",
        &issues("open"),
        Some(&second_token),
        Value::Null,
        200,
    )
    .await;
    let (_, active) = request(
        &router,
        "orgs/list-pat-grants",
        "GET",
        grants,
        Some(&admin),
        Value::Null,
        200,
    )
    .await;
    assert!(
        active
            .as_array()
            .unwrap()
            .iter()
            .all(|grant| grant["owner"]["login"] != "carol"),
        "{active}"
    );

    // Denying every pending request in bulk records the reason for the user.
    let (denied, dave_token, _) = fine(f.pool(), dave, &other.session, org, 30).await.unwrap();
    request(
        &router,
        "orgs/review-pat-grant-requests-in-bulk",
        "POST",
        requests,
        Some(&admin),
        json!({"action":"deny","reason":"Use the deploy app"}),
        202,
    )
    .await;
    let token = db::fine_grained_tokens::get(f.pool(), denied)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (token.status.as_str(), token.reason.as_deref()),
        ("denied", Some("Use the deploy app"))
    );
    request(
        &router,
        "issues/list-for-repo",
        "GET",
        &issues("internal"),
        Some(&dave_token),
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/review-pat-grant-requests-in-bulk",
        "POST",
        requests,
        Some(&admin),
        json!({"action":"maybe"}),
        422,
    )
    .await;

    // Refusing classic tokens leaves fine-grained tokens and sessions working.
    let dave_classic = classic(f.pool(), dave, &other.session).await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/internal",
        Some(&dave_classic),
        Value::Null,
        200,
    )
    .await;
    let reply = owner
        .form_reply(
            &settings,
            &[("action", "token-policy"), ("fine_grained_allowed", "true")],
        )
        .await;
    assert_eq!(reply.status, 303, "{}", reply.text());
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/internal",
        Some(&dave_classic),
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "issues/list-for-repo",
        "GET",
        &issues("secret"),
        Some(&owned),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(other.page("/acme/internal").await.0, 200);
    f.finish().await;
}

#[tokio::test]
async fn sqlite() {
    contract(false).await;
}

#[tokio::test]
async fn postgres() {
    contract(true).await;
}

//! Organization installation discovery and who may manage fine-grained token grants, on both
//! engines.
use super::request;
use crate::support::{database::Database, http_client::Client, organizations::applied};
use codeyard::backend::{
    db::{
        self,
        models::OrgRole,
        organizations::{self, NewOrganization, OrganizationProfile},
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
    let admin = f.actor("admin").await;
    let outsider = f.actor("outsider").await;
    let mut orgs = Vec::new();
    for name in ["acme", "foreign", "empty"] {
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
    for (user, role) in [(member, OrgRole::Member), (admin, OrgRole::Admin)] {
        applied(
            organizations::add_organization_member(f.pool(), owner, orgs[0].id, user, role)
                .await
                .unwrap(),
        );
    }
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), owner).await;
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
    let mut apps = Vec::new();
    for slug in ["first-app", "second-app"] {
        let response = browser
            .form_reply(
                "/settings/apps/new",
                &[
                    ("name", slug),
                    ("slug", slug),
                    ("homepage", "https://example.test/app"),
                    ("permissions", r#"{"contents":"read"}"#),
                    ("events", "push"),
                ],
            )
            .await;
        assert_eq!(response.status, 200, "{}", response.text());
        apps.push(db::github_apps::app(f.pool(), slug).await.unwrap().unwrap());
    }
    let mut installations = Vec::new();
    for (app, user, org, selected) in [
        (apps[0].id, None, Some(orgs[1].id), false),
        (apps[0].id, Some(owner), None, false),
        (apps[0].id, None, Some(orgs[0].id), false),
        (apps[1].id, None, Some(orgs[0].id), true),
    ] {
        installations.push(
            db::github_apps::install(
                f.pool(),
                owner,
                &browser.session,
                app,
                user,
                org,
                if selected { Some(&[]) } else { None },
            )
            .await
            .unwrap()
            .unwrap(),
        );
    }
    assert!(db::github_apps::change_installation(
        f.pool(),
        apps[1].id,
        installations[3].id,
        "suspend"
    )
    .await
    .unwrap());

    let operation = "orgs/list-app-installations";
    let path = "/api/v3/orgs/acme/installations";
    for (token, status) in [
        (None, 401),
        (Some(credentials[2].as_str()), 403),
        (Some(credentials[3].as_str()), 403),
        (Some(credentials[4].as_str()), 403),
    ] {
        request(&router, operation, "GET", path, token, Value::Null, status).await;
    }
    for token in [&credentials[0], &credentials[1]] {
        let (_, body) = request(
            &router,
            operation,
            "GET",
            path,
            Some(token),
            Value::Null,
            200,
        )
        .await;
        assert_eq!(body["total_count"], 2);
        assert_eq!(body["installations"].as_array().unwrap().len(), 2);
        let first = &body["installations"][0];
        assert_eq!(first["id"], installations[2].id);
        assert_eq!(first["app_id"], apps[0].id);
        assert_eq!(first["app_slug"], "first-app");
        assert_eq!(first["account"]["login"], "acme");
        assert_eq!(first["target_id"], orgs[0].id);
        assert_eq!(first["target_type"], "Organization");
        assert_eq!(first["repository_selection"], "all");
        assert_eq!(first["permissions"]["contents"], "read");
        assert_eq!(first["events"], json!(["push"]));
        assert_eq!(first["suspended_at"], Value::Null);
        assert_eq!(body["installations"][1]["repository_selection"], "selected");
        assert!(body["installations"][1]["suspended_at"].is_string());
    }
    for (page, id, next) in [
        (1, Some(installations[2].id), true),
        (2, Some(installations[3].id), false),
        (3, None, false),
    ] {
        let (headers, body) = request(
            &router,
            operation,
            "GET",
            &format!("{path}?per_page=1&page={page}"),
            Some(&credentials[0]),
            Value::Null,
            200,
        )
        .await;
        assert_eq!(body["total_count"], 2);
        assert_eq!(
            body["installations"].as_array().unwrap().len(),
            usize::from(id.is_some())
        );
        if let Some(id) = id {
            assert_eq!(body["installations"][0]["id"], id);
        }
        let link = headers
            .get("link")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        assert_eq!(link.contains("rel=\"next\""), next);
        if next {
            assert!(link.contains("page=2"));
            assert!(link.contains("per_page=1"));
        }
    }
    let (_, empty) = request(
        &router,
        operation,
        "GET",
        "/api/v3/orgs/empty/installations",
        Some(&credentials[0]),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(empty, json!({"total_count":0,"installations":[]}));
    request(
        &router,
        operation,
        "GET",
        "/api/v3/orgs/missing/installations",
        Some(&credentials[0]),
        Value::Null,
        404,
    )
    .await;

    let before = db::users::tokens::list_tokens(f.pool(), owner, 0)
        .await
        .unwrap();
    // Only owners manage token grants; with no fine-grained tokens the lists are empty and a
    // named grant or request is missing.
    for (operation, method, suffix, body, found) in [
        (
            "orgs/list-pat-grants",
            "GET",
            "personal-access-tokens",
            Value::Null,
            200,
        ),
        (
            "orgs/list-pat-grant-repositories",
            "GET",
            "personal-access-tokens/1/repositories",
            Value::Null,
            404,
        ),
        (
            "orgs/list-pat-grant-requests",
            "GET",
            "personal-access-token-requests",
            Value::Null,
            200,
        ),
        (
            "orgs/list-pat-grant-request-repositories",
            "GET",
            "personal-access-token-requests/1/repositories",
            Value::Null,
            404,
        ),
        (
            "orgs/review-pat-grant-request",
            "POST",
            "personal-access-token-requests/1",
            json!({"action":"approve"}),
            404,
        ),
        (
            "orgs/review-pat-grant-requests-in-bulk",
            "POST",
            "personal-access-token-requests",
            json!({"action":"deny","pat_request_ids":[1]}),
            404,
        ),
        (
            "orgs/update-pat-access",
            "POST",
            "personal-access-tokens/1",
            json!({"action":"revoke"}),
            404,
        ),
        (
            "orgs/update-pat-accesses",
            "POST",
            "personal-access-tokens",
            json!({"action":"revoke","pat_ids":[1]}),
            404,
        ),
    ] {
        let path = format!("/api/v3/orgs/acme/{suffix}");
        let read_only = if method == "POST" { 403 } else { found };
        for (token, status) in [
            (None, 401),
            (Some(credentials[2].as_str()), 403),
            (Some(credentials[3].as_str()), 403),
            (Some(credentials[4].as_str()), 403),
            (Some(credentials[1].as_str()), read_only),
            (Some(credentials[0].as_str()), found),
        ] {
            let (_, reply) = request(
                &router,
                operation,
                method,
                &path,
                token,
                body.clone(),
                status,
            )
            .await;
            if status == 200 {
                assert_eq!(reply, json!([]));
                continue;
            }
            assert_eq!(reply["status"], status.to_string());
            assert!(reply["documentation_url"].is_string());
            if status == 404 {
                // The implemented handlers answer with the API's standard error document.
                assert_eq!(reply["message"], "Not Found");
                assert_eq!(
                    reply["documentation_url"],
                    "https://docs.github.com/en/rest"
                );
            }
        }
    }
    let after = db::users::tokens::list_tokens(f.pool(), owner, 0)
        .await
        .unwrap();
    assert_eq!(
        before.iter().map(|t| t.id).collect::<Vec<_>>(),
        after.iter().map(|t| t.id).collect::<Vec<_>>()
    );
    let successor = f.actor("successor").await;
    applied(
        organizations::add_organization_member(
            f.pool(),
            owner,
            orgs[0].id,
            successor,
            OrgRole::Owner,
        )
        .await
        .unwrap(),
    );
    f.execute(
        "UPDATE org_memberships SET role='member' WHERE org_id=$1 AND user_id=$2",
        &[orgs[0].id, owner],
    )
    .await;
    request(
        &router,
        operation,
        "GET",
        path,
        Some(&credentials[0]),
        Value::Null,
        403,
    )
    .await;
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

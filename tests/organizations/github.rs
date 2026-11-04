//! Organization REST documents and privacy through the real router on both engines.
mod blocks;
mod dependabot_access;
mod hooks;
mod installations;
mod invitations;
mod issue_fields;
mod issue_types;
mod outside;
mod private_registries;
mod properties;
mod release_settings;
mod repo_rulesets;
mod roles;
mod rulesets;
mod security;
mod settings;
mod teams;
mod token_grants;
use crate::support::{
    codeyard_db_orgs::organizations::applied, database::Database,
    github_spec::assert_github_schema, http_client::Client,
};
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request},
    Router,
};
use codeyard::backend::api::create_router;
use codeyard::backend::app::AppState;
use codeyard::backend::ci::CiConfig;

use codeyard_db_orgs::organizations::NewOrganization;
use codeyard_db_orgs::organizations::OrganizationProfile;

use codeyard::backend::storage::ObjectStore;
use codeyard_db_core::models::OrgRole;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

async fn request(
    router: &Router,
    operation: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
    expected: u16,
) -> (HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", token);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status,
        expected,
        "{operation}: {}",
        String::from_utf8_lossy(&bytes)
    );
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        let value = serde_json::from_slice(&bytes).unwrap();
        assert_github_schema(operation, status, &value);
        value
    };
    (headers, value)
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let outsider = f.actor("outsider").await;
    let org = applied(
        codeyard_db_orgs::organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new(
                "acme",
                OrganizationProfile::new(Some("Acme"), Some("Tools")).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        codeyard_db_orgs::organizations::add_organization_member(
            f.pool(),
            owner,
            org.id,
            member,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        codeyard_db_orgs::organizations::invite_organization_member(
            f.pool(),
            owner,
            org.id,
            outsider,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    let second = applied(
        codeyard_db_orgs::organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("second", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let mut credentials = Vec::new();
    for (actor, scope) in [
        (owner, "write"),
        (member, "write"),
        (outsider, "write"),
        (member, "read"),
    ] {
        let session = Client::login(&router, f.pool(), actor).await;
        let token = codeyard_identity::tokens::create(
            f.pool(),
            actor,
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
    let member_token = Some(credentials[1].as_str());
    let other_token = Some(credentials[2].as_str());
    let read_token = Some(credentials[3].as_str());
    let (_, body) = request(
        &router,
        "orgs/get",
        "GET",
        "/api/v3/orgs/acme",
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["name"], "Acme");
    assert_eq!(body["description"], "Tools");
    assert_eq!(body["public_repos"], 0);
    assert!(body.get("billing_email").is_none());
    assert!(body["url"].as_str().unwrap().ends_with("/api/v3/orgs/acme"));
    request(
        &router,
        "orgs/get",
        "GET",
        "/api/v3/orgs/missing",
        None,
        Value::Null,
        404,
    )
    .await;
    let (headers, body) = request(
        &router,
        "orgs/list",
        "GET",
        "/api/v3/organizations?per_page=1",
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["id"], org.id);
    assert!(headers["link"]
        .to_str()
        .unwrap()
        .contains(&format!("since={}", org.id)));
    let (_, body) = request(
        &router,
        "orgs/list",
        "GET",
        &format!("/api/v3/organizations?since={}", org.id),
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["id"], second.id);
    let (_, body) = request(
        &router,
        "orgs/list-for-authenticated-user",
        "GET",
        "/api/v3/user/orgs",
        member_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    request(
        &router,
        "orgs/list-for-authenticated-user",
        "GET",
        "/api/v3/user/orgs",
        None,
        Value::Null,
        401,
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/list-for-user",
        "GET",
        "/api/v3/users/member/orgs",
        member_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body, json!([]));
    for (token, count) in [(None, 0), (other_token, 0), (member_token, 2)] {
        let (_, body) = request(
            &router,
            "orgs/list-members",
            "GET",
            "/api/v3/orgs/acme/members",
            token,
            Value::Null,
            200,
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), count);
    }
    let (headers, body) = request(
        &router,
        "orgs/list-members",
        "GET",
        "/api/v3/orgs/acme/members?per_page=1",
        member_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["login"], "member");
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let (_, body) = request(
        &router,
        "orgs/list-members",
        "GET",
        "/api/v3/orgs/acme/members?role=admin",
        member_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["id"], owner);
    request(
        &router,
        "orgs/list-members",
        "GET",
        "/api/v3/orgs/acme/members?filter=2fa_disabled",
        other_token,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "orgs/list-members",
        "GET",
        "/api/v3/orgs/acme/members?filter=2fa_disabled",
        member_token,
        Value::Null,
        403,
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/list-members",
        "GET",
        "/api/v3/orgs/acme/members?filter=2fa_disabled",
        owner_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 2);
    let (_, body) = request(
        &router,
        "orgs/list-members",
        "GET",
        "/api/v3/orgs/acme/members?filter=2fa_insecure",
        owner_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body, json!([]));
    request(
        &router,
        "orgs/list-members",
        "GET",
        "/api/v3/orgs/acme/members?role=invalid",
        member_token,
        Value::Null,
        422,
    )
    .await;
    request(
        &router,
        "orgs/check-membership-for-user",
        "GET",
        "/api/v3/orgs/acme/members/member",
        owner_token,
        Value::Null,
        204,
    )
    .await;
    let (headers, _) = request(
        &router,
        "orgs/check-membership-for-user",
        "GET",
        "/api/v3/orgs/acme/members/member",
        None,
        Value::Null,
        302,
    )
    .await;
    assert!(headers["location"]
        .to_str()
        .unwrap()
        .ends_with("/api/v3/orgs/acme/public_members/member"));
    request(
        &router,
        "orgs/check-public-membership-for-user",
        "GET",
        "/api/v3/orgs/acme/public_members/member",
        None,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/set-public-membership-for-authenticated-user",
        "PUT",
        "/api/v3/orgs/acme/public_members/member",
        read_token,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "orgs/set-public-membership-for-authenticated-user",
        "PUT",
        "/api/v3/orgs/acme/public_members/owner",
        member_token,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "orgs/set-public-membership-for-authenticated-user",
        "PUT",
        "/api/v3/orgs/acme/public_members/member",
        other_token,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "orgs/set-public-membership-for-authenticated-user",
        "PUT",
        "/api/v3/orgs/acme/public_members/member",
        member_token,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/check-public-membership-for-user",
        "GET",
        "/api/v3/orgs/acme/public_members/member",
        None,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/check-membership-for-user",
        "GET",
        "/api/v3/orgs/acme/members/member",
        None,
        Value::Null,
        204,
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/list-for-user",
        "GET",
        "/api/v3/users/member/orgs",
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["id"], org.id);
    let (_, body) = request(
        &router,
        "orgs/list-public-members",
        "GET",
        "/api/v3/orgs/acme/public_members",
        owner_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], member);
    request(
        &router,
        "orgs/remove-public-membership-for-authenticated-user",
        "DELETE",
        "/api/v3/orgs/acme/public_members/member",
        member_token,
        Value::Null,
        204,
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/get-membership-for-user",
        "GET",
        "/api/v3/orgs/acme/memberships/owner",
        member_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["role"], "admin");
    assert_eq!(body["state"], "active");
    request(
        &router,
        "orgs/get-membership-for-user",
        "GET",
        "/api/v3/orgs/acme/memberships/owner",
        other_token,
        Value::Null,
        404,
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/get-membership-for-authenticated-user",
        "GET",
        "/api/v3/user/memberships/orgs/acme",
        other_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["state"], "pending");
    let (_, body) = request(
        &router,
        "orgs/list-memberships-for-authenticated-user",
        "GET",
        "/api/v3/user/memberships/orgs?state=pending",
        other_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    request(
        &router,
        "orgs/update-membership-for-authenticated-user",
        "PATCH",
        "/api/v3/user/memberships/orgs/acme",
        other_token,
        json!({"state":"pending"}),
        422,
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/update-membership-for-authenticated-user",
        "PATCH",
        "/api/v3/user/memberships/orgs/acme",
        other_token,
        json!({"state":"active"}),
        200,
    )
    .await;
    assert_eq!(body["state"], "active");
    let (_, body) = request(
        &router,
        "orgs/list-memberships-for-authenticated-user",
        "GET",
        "/api/v3/user/memberships/orgs?state=active",
        other_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["organization"]["id"], org.id);
    request(
        &router,
        "orgs/set-membership-for-user",
        "PUT",
        "/api/v3/orgs/acme/memberships/owner",
        owner_token,
        json!({"role":"member"}),
        422,
    )
    .await;
    request(
        &router,
        "orgs/set-membership-for-user",
        "PUT",
        "/api/v3/orgs/acme/memberships/outsider",
        read_token,
        json!({"role":"admin"}),
        403,
    )
    .await;
    applied(
        codeyard_db_orgs::organizations::update_organization_member_role(
            f.pool(),
            owner,
            org.id,
            member,
            OrgRole::Admin,
        )
        .await
        .unwrap(),
    );
    request(
        &router,
        "orgs/set-membership-for-user",
        "PUT",
        "/api/v3/orgs/acme/memberships/outsider",
        member_token,
        json!({"role":"member"}),
        403,
    )
    .await;
    request(
        &router,
        "orgs/remove-membership-for-user",
        "DELETE",
        "/api/v3/orgs/acme/memberships/outsider",
        member_token,
        Value::Null,
        403,
    )
    .await;
    applied(
        codeyard_db_orgs::organizations::update_organization_member_role(
            f.pool(),
            owner,
            org.id,
            member,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    request(
        &router,
        "orgs/remove-membership-for-user",
        "DELETE",
        "/api/v3/orgs/acme/memberships/outsider",
        owner_token,
        Value::Null,
        204,
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/set-membership-for-user",
        "PUT",
        "/api/v3/orgs/acme/memberships/outsider",
        owner_token,
        json!({"role":"admin"}),
        200,
    )
    .await;
    assert_eq!(body["state"], "pending");
    assert_eq!(body["role"], "admin");
    let (_, body) = request(
        &router,
        "orgs/set-membership-for-user",
        "PUT",
        "/api/v3/orgs/acme/memberships/outsider",
        owner_token,
        json!({}),
        200,
    )
    .await;
    assert_eq!(body["state"], "pending");
    assert_eq!(body["role"], "member");
    request(
        &router,
        "orgs/remove-membership-for-user",
        "DELETE",
        "/api/v3/orgs/acme/memberships/outsider",
        owner_token,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/get-membership-for-authenticated-user",
        "GET",
        "/api/v3/user/memberships/orgs/acme",
        other_token,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/set-membership-for-user",
        "PUT",
        "/api/v3/orgs/acme/memberships/outsider",
        owner_token,
        json!({"role":"invalid"}),
        422,
    )
    .await;
    request(
        &router,
        "orgs/set-membership-for-user",
        "PUT",
        "/api/v3/orgs/acme/memberships/outsider",
        owner_token,
        json!({"role":"admin"}),
        200,
    )
    .await;
    request(
        &router,
        "orgs/update-membership-for-authenticated-user",
        "PATCH",
        "/api/v3/user/memberships/orgs/acme",
        other_token,
        json!({"state":"active"}),
        200,
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/set-membership-for-user",
        "PUT",
        "/api/v3/orgs/acme/memberships/outsider",
        owner_token,
        json!({"role":"member"}),
        200,
    )
    .await;
    assert_eq!(body["state"], "active");
    assert_eq!(body["role"], "member");
    request(
        &router,
        "orgs/remove-member",
        "DELETE",
        "/api/v3/orgs/acme/members/outsider",
        owner_token,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/remove-member",
        "DELETE",
        "/api/v3/orgs/acme/members/outsider",
        owner_token,
        Value::Null,
        204,
    )
    .await;
    let browser = Client::login(&router, f.pool(), member).await;
    let path = format!("/settings/organizations/{}?tab=members", org.id);
    let reply = browser
        .exchange("GET", &path, "text/plain", Vec::new())
        .await;
    assert_eq!(reply.status.as_u16(), 200, "{}", reply.text());
    assert!(reply.text().contains("Your membership visibility"));
    let reply = browser
        .exchange(
            "POST",
            &path,
            "application/x-www-form-urlencoded",
            format!(
                "csrf_token={}&action=membership-visibility&visibility=public",
                browser.csrf
            )
            .into_bytes(),
        )
        .await;
    assert_eq!(reply.status.as_u16(), 303, "{}", reply.text());
    request(
        &router,
        "orgs/check-public-membership-for-user",
        "GET",
        "/api/v3/orgs/acme/public_members/member",
        None,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/delete",
        "DELETE",
        "/api/v3/orgs/second",
        read_token,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "orgs/delete",
        "DELETE",
        "/api/v3/orgs/second",
        member_token,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "orgs/delete",
        "DELETE",
        "/api/v3/orgs/second",
        owner_token,
        Value::Null,
        202,
    )
    .await;
    request(
        &router,
        "orgs/get",
        "GET",
        "/api/v3/orgs/second",
        None,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/delete",
        "DELETE",
        "/api/v3/orgs/second",
        owner_token,
        Value::Null,
        404,
    )
    .await;
    f.execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[owner])
        .await;
    request(
        &router,
        "orgs/get",
        "GET",
        "/api/v3/orgs/acme",
        None,
        Value::Null,
        404,
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/list",
        "GET",
        "/api/v3/organizations",
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body, json!([]));
    f.finish().await;
}

#[tokio::test]
async fn github_organizations_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn github_organizations_postgres() {
    contract(true).await;
}

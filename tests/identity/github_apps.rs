//! App JWTs, installation credentials and native account consent through the real router.
use super::github::request;
use crate::support::{
    database::Database,
    http_client::{visible_text, Client},
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use codeyard::backend::{
    db,
    identity::{
        oauth_apps,
        tokens::{self, TokenInput},
    },
};
use rsa::{
    pkcs1v15::SigningKey,
    pkcs8::DecodePrivateKey,
    signature::{SignatureEncoding, Signer},
    RsaPrivateKey,
};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn registry_denied(router: &axum::Router, value: &str) {
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/v2/token?service=codeyard")
                .header("Authorization", format!("Bearer {value}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        403,
        "Integration credentials cannot acquire account-wide registry grants"
    );
}
pub(super) fn jwt(key: &RsaPrivateKey, issuer: Value, expires: i64) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let body = URL_SAFE_NO_PAD.encode(
        json!({"iss":issuer,"iat":chrono::Utc::now().timestamp()-10,"exp":expires}).to_string(),
    );
    let signed = format!("{header}.{body}");
    let signature = SigningKey::<sha2::Sha256>::new(key.clone()).sign(signed.as_bytes());
    format!(
        "Bearer {signed}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_vec())
    )
}
async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let repo = db::repositories::records::create_repository(
        f.pool(),
        alice,
        "proj",
        None,
        true,
        "alice/proj",
    )
    .await
    .unwrap()
    .id;
    let excluded = db::repositories::records::create_repository(
        f.pool(),
        alice,
        "excluded",
        None,
        true,
        "alice/excluded",
    )
    .await
    .unwrap()
    .id;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let user = Client::login(&router, f.pool(), alice).await;
    let other = Client::login(&router, f.pool(), bob).await;
    let response = user.page_ok("/settings/apps/new").await;
    assert!(response.contains("method=\"post\""));
    let response = user
        .form_reply(
            "/settings/apps/new",
            &[
                ("name", "Builder App"),
                ("slug", "builder-app"),
                ("homepage", "https://example.test/callback"),
                (
                    "permissions",
                    r#"{"contents":"write","issues":"write","statuses":"read"}"#,
                ),
                ("events", "push,issues"),
            ],
        )
        .await;
    assert_eq!(response.status, 200, "{}", response.text());
    let text = visible_text(&response.text());
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----{}-----END PRIVATE KEY-----",
        text.split("-----BEGIN PRIVATE KEY-----")
            .nth(1)
            .unwrap()
            .split("-----END PRIVATE KEY-----")
            .next()
            .unwrap()
    );
    let key = RsaPrivateKey::from_pkcs8_pem(&pem).unwrap();
    let secret = text
        .split("Client secret</dt>")
        .nth(1)
        .unwrap()
        .split("</dd>")
        .next()
        .unwrap()
        .rsplit('>')
        .next()
        .unwrap()
        .to_owned();
    let app = db::github_apps::app(f.pool(), "builder-app")
        .await
        .unwrap()
        .unwrap();
    let auth = jwt(&key, json!(app.id), chrono::Utc::now().timestamp() + 300);
    let (_, document) = request(
        &router,
        "apps/get-authenticated",
        "GET",
        "/api/v3/app",
        Some(&auth),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(document["slug"], "builder-app");
    request(
        &router,
        "apps/get-authenticated",
        "GET",
        "/api/v3/app",
        Some(&jwt(
            &key,
            json!(app.client_id),
            chrono::Utc::now().timestamp() - 1,
        )),
        Value::Null,
        401,
    )
    .await;
    request(
        &router,
        "apps/get-authenticated",
        "GET",
        "/api/v3/app",
        Some("Bearer invalid.jwt.signature"),
        Value::Null,
        401,
    )
    .await;
    request(
        &router,
        "apps/get-by-slug",
        "GET",
        "/api/v3/apps/builder-app",
        None,
        Value::Null,
        200,
    )
    .await;
    let response = other
        .form_reply(
            "/apps/builder-app/installations/new",
            &[("repository_ids", &repo.to_string())],
        )
        .await;
    assert_eq!(response.status, 403);
    let response = user
        .form_reply(
            "/apps/builder-app/installations/new",
            &[("repository_ids", &repo.to_string())],
        )
        .await;
    assert_eq!(response.status, 303, "{}", response.text());
    let (_, rows) = request(
        &router,
        "apps/list-installations",
        "GET",
        "/api/v3/app/installations?per_page=1",
        Some(&auth),
        Value::Null,
        200,
    )
    .await;
    let id = rows[0]["id"].as_i64().unwrap();
    for (operation, path) in [
        (
            "apps/get-installation",
            format!("/api/v3/app/installations/{id}"),
        ),
        (
            "apps/get-repo-installation",
            "/api/v3/repos/alice/proj/installation".into(),
        ),
        (
            "apps/get-user-installation",
            "/api/v3/users/alice/installation".into(),
        ),
    ] {
        request(
            &router,
            operation,
            "GET",
            &path,
            Some(&auth),
            Value::Null,
            200,
        )
        .await;
    }
    let (_, status_only) = request(
        &router,
        "apps/create-installation-access-token",
        "POST",
        &format!("/api/v3/app/installations/{id}/access_tokens"),
        Some(&auth),
        json!({"permissions":{"statuses":"read"}}),
        201,
    )
    .await;
    request(
        &router,
        "repos/get-commit",
        "GET",
        "/api/v3/repos/alice/proj/commits/status",
        Some(&format!(
            "Bearer {}",
            status_only["token"].as_str().unwrap()
        )),
        Value::Null,
        403,
    )
    .await;
    registry_denied(&router, status_only["token"].as_str().unwrap()).await;
    let (_, token) = request(
        &router,
        "apps/create-installation-access-token",
        "POST",
        &format!("/api/v3/app/installations/{id}/access_tokens"),
        Some(&auth),
        json!({"permissions":{"contents":"read"},"repository_ids":[repo]}),
        201,
    )
    .await;
    let access = format!("Bearer {}", token["token"].as_str().unwrap());
    request(
        &router,
        "apps/list-repos-accessible-to-installation",
        "GET",
        "/api/v3/installation/repositories",
        Some(&access),
        Value::Null,
        200,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/alice/proj",
        Some(&access),
        Value::Null,
        200,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/alice/excluded",
        Some(&access),
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "issues/create",
        "POST",
        "/api/v3/repos/alice/proj/issues",
        Some(&access),
        json!({"title":"not permitted"}),
        403,
    )
    .await;
    request(
        &router,
        "apps/create-installation-access-token",
        "POST",
        &format!("/api/v3/app/installations/{id}/access_tokens"),
        Some(&auth),
        json!({"permissions":{"administration":"write"}}),
        422,
    )
    .await;
    request(
        &router,
        "apps/create-installation-access-token",
        "POST",
        &format!("/api/v3/app/installations/{id}/access_tokens"),
        Some(&auth),
        json!({"repository_ids":[excluded]}),
        422,
    )
    .await;
    let account = tokens::create(
        f.pool(),
        alice,
        Some(&user.session),
        TokenInput {
            name: "installations",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let account = format!("Bearer {}", account.value);
    request(
        &router,
        "apps/list-installations-for-authenticated-user",
        "GET",
        "/api/v3/user/installations",
        Some(&account),
        Value::Null,
        200,
    )
    .await;
    request(
        &router,
        "apps/list-installation-repos-for-authenticated-user",
        "GET",
        &format!("/api/v3/user/installations/{id}/repositories"),
        Some(&account),
        Value::Null,
        200,
    )
    .await;
    request(
        &router,
        "apps/add-repo-to-installation-for-authenticated-user",
        "PUT",
        &format!("/api/v3/user/installations/{id}/repositories/{excluded}"),
        Some(&account),
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "apps/remove-repo-from-installation-for-authenticated-user",
        "DELETE",
        &format!("/api/v3/user/installations/{id}/repositories/{repo}"),
        Some(&account),
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/alice/proj",
        Some(&access),
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "apps/revoke-installation-access-token",
        "DELETE",
        "/api/v3/installation/token",
        Some(&access),
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "apps/list-repos-accessible-to-installation",
        "GET",
        "/api/v3/installation/repositories",
        Some(&access),
        Value::Null,
        401,
    )
    .await;
    let org_input = db::organizations::NewOrganization::new(
        "app-org",
        db::organizations::OrganizationProfile::new(None, None).unwrap(),
    )
    .unwrap();
    let org = match db::organizations::profiles::create_organization(f.pool(), alice, &org_input)
        .await
        .unwrap()
    {
        db::organizations::OrganizationOutcome::Applied(org) => org,
        _ => panic!("organization creation"),
    };
    assert!(matches!(
        db::organizations::membership::add_organization_member(
            f.pool(),
            alice,
            org.id,
            bob,
            db::models::OrgRole::Member
        )
        .await
        .unwrap(),
        db::organizations::OrganizationOutcome::Applied(_)
    ));
    let response = other
        .form_reply(
            "/apps/builder-app/installations/new",
            &[
                ("organization_id", &org.id.to_string()),
                ("decision", "request"),
            ],
        )
        .await;
    assert_eq!(response.status, 303, "{}", response.text());
    let (_, requests) = request(
        &router,
        "apps/list-installation-requests-for-authenticated-app",
        "GET",
        "/api/v3/app/installation-requests",
        Some(&auth),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(requests.as_array().unwrap().len(), 1);
    assert_eq!(requests[0]["requester"]["login"], "bob");
    let response = user
        .form_reply(
            "/apps/builder-app/installations/new",
            &[("organization_id", &org.id.to_string())],
        )
        .await;
    assert_eq!(response.status, 303, "{}", response.text());
    request(
        &router,
        "apps/get-org-installation",
        "GET",
        "/api/v3/orgs/app-org/installation",
        Some(&auth),
        Value::Null,
        200,
    )
    .await;
    let (_, requests) = request(
        &router,
        "apps/list-installation-requests-for-authenticated-app",
        "GET",
        "/api/v3/app/installation-requests",
        Some(&auth),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(requests, json!([]));
    // A real OAuth exchange supplies an app-owned user credential for narrowing.
    let code = oauth_apps::random();
    db::oauth_apps::save_grant(
        f.pool(),
        &db::oauth_apps::Grant {
            hash: tokens::token_hash(&code),
            client_id: app.client_id.clone(),
            user_code: oauth_apps::random(),
            scopes: "repo".into(),
            expires_at: chrono::Utc::now().timestamp() + 60,
            interval: 5,
            last_poll: 0,
            user_id: Some(alice),
            session_id: Some(user.session.clone()),
            denied: false,
            kind: "web".into(),
            redirect_uri: app.homepage.clone(),
            state: String::new(),
            challenge: String::new(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        db::oauth_apps::exchange(
            f.pool(),
            &tokens::token_hash(&code),
            &app.client_id,
            "web",
            Some(&app.homepage),
            "",
            Some(""),
            "user-app-token"
        )
        .await
        .unwrap(),
        db::oauth_apps::Exchange::Token(_)
    ));
    registry_denied(&router, "user-app-token").await;
    let parent_expiry = chrono::Utc::now() + chrono::Duration::minutes(5);
    f.execute(
        &format!(
            "UPDATE personal_access_tokens SET expires_at='{}' WHERE token_hash='{}'",
            parent_expiry.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            tokens::token_hash("user-app-token")
        ),
        &[],
    )
    .await;
    let basic = format!(
        "Basic {}",
        STANDARD.encode(format!("{}:{secret}", app.client_id))
    );
    request(
        &router,
        "apps/scope-token",
        "POST",
        &format!("/api/v3/applications/{}/token/scoped", app.client_id),
        Some(&basic),
        json!({"access_token":"user-app-token","target":"alice","target_id":-1}),
        422,
    )
    .await;
    if org.id == alice {
        request(
            &router,
            "apps/scope-token",
            "POST",
            &format!("/api/v3/applications/{}/token/scoped", app.client_id),
            Some(&basic),
            json!({"access_token":"user-app-token","target_id":alice}),
            422,
        )
        .await;
    }
    let(_,scoped)=request(&router,"apps/scope-token","POST",&format!("/api/v3/applications/{}/token/scoped",app.client_id),Some(&basic),json!({"access_token":"user-app-token","target":"alice","repository_ids":[excluded],"permissions":{"contents":"read"}}),200).await;
    assert!(scoped["token"].as_str().unwrap().starts_with("ghu_"));
    let expiry =
        chrono::DateTime::parse_from_rfc3339(scoped["expires_at"].as_str().unwrap()).unwrap();
    assert!(expiry.with_timezone(&chrono::Utc) <= parent_expiry);
    let (_, installations) = request(
        &router,
        "apps/list-installations-for-authenticated-user",
        "GET",
        "/api/v3/user/installations",
        Some(&format!("Bearer {}", scoped["token"].as_str().unwrap())),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(installations["total_count"], 1);
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/alice/excluded",
        Some(&format!("Bearer {}", scoped["token"].as_str().unwrap())),
        Value::Null,
        200,
    )
    .await;
    // Account deletion cascades installations, but must retain a revoked scoped-token marker.
    let (_, org_scoped) = request(
        &router,
        "apps/scope-token",
        "POST",
        &format!("/api/v3/applications/{}/token/scoped", app.client_id),
        Some(&basic),
        json!({"access_token":"user-app-token","target":"app-org","permissions":{}}),
        200,
    )
    .await;
    f.execute("DELETE FROM organizations WHERE id=$1", &[org.id])
        .await;
    for (operation, method, path) in [
        (
            "apps/check-token",
            "POST",
            format!("/api/v3/applications/{}/token", app.client_id),
        ),
        (
            "apps/reset-token",
            "PATCH",
            format!("/api/v3/applications/{}/token", app.client_id),
        ),
        (
            "apps/scope-token",
            "POST",
            format!("/api/v3/applications/{}/token/scoped", app.client_id),
        ),
    ] {
        request(
            &router,
            operation,
            method,
            &path,
            Some(&basic),
            json!({"access_token":org_scoped["token"],"target":"alice","repository_ids":[repo]}),
            404,
        )
        .await;
    }
    request(
        &router,
        "users/get-authenticated",
        "GET",
        "/api/v3/user",
        Some(&format!("Bearer {}", org_scoped["token"].as_str().unwrap())),
        Value::Null,
        401,
    )
    .await;
    request(
        &router,
        "apps/suspend-installation",
        "PUT",
        &format!("/api/v3/app/installations/{id}/suspended"),
        Some(&auth),
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/alice/excluded",
        Some(&format!("Bearer {}", scoped["token"].as_str().unwrap())),
        Value::Null,
        401,
    )
    .await;
    request(
        &router,
        "apps/unsuspend-installation",
        "DELETE",
        &format!("/api/v3/app/installations/{id}/suspended"),
        Some(&auth),
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "apps/delete-installation",
        "DELETE",
        &format!("/api/v3/app/installations/{id}"),
        Some(&auth),
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "apps/get-installation",
        "GET",
        &format!("/api/v3/app/installations/{id}"),
        Some(&auth),
        Value::Null,
        404,
    )
    .await;
    // Removing the bot identity must not reclassify its user credentials as classic OAuth.
    f.execute("DELETE FROM users WHERE id=$1", &[app.bot_id])
        .await;
    assert!(db::oauth_apps::application(f.pool(), &app.client_id)
        .await
        .unwrap()
        .is_some());
    request(
        &router,
        "users/get-authenticated",
        "GET",
        "/api/v3/user",
        Some("Bearer user-app-token"),
        Value::Null,
        401,
    )
    .await;
    assert!(oauth_apps::client(f.pool(), &app.client_id)
        .await
        .unwrap()
        .is_none());
}
#[tokio::test]
async fn github_apps_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn github_apps_postgres() {
    contract(true).await;
}

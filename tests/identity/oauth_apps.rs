//! GitHub CLI OAuth requests, native approval, replay and revocation on both databases.
use crate::support::{database::Database, http_client::Client};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use codeyard::backend::{
    db,
    identity::{oauth_apps as oauth, tokens::token_hash},
};
use serde_json::{json, Value};
use tower::ServiceExt;

const CLI: &str = oauth::CLI_CLIENT;
const SECRET: &str = "34ddeff2b558a23d38fba8a6de74f086ede1cc0b";
const DEVICE: &str = "urn:ietf:params:oauth:grant-type:device_code";

async fn post(router: &Router, path: &str, fields: &[(&str, &str)], json: bool) -> Value {
    let mut body = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in fields {
        body.append_pair(key, value);
    }
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .header(
                    "accept",
                    if json {
                        "application/json"
                    } else {
                        "application/x-www-form-urlencoded"
                    },
                )
                .body(Body::from(body.finish()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store, private");
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    if json {
        serde_json::from_slice(&bytes).unwrap()
    } else {
        let fields: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(&bytes).into_owned().collect();
        serde_json::to_value(fields).unwrap()
    }
}

async fn device(router: &Router, scopes: &str) -> Value {
    post(
        router,
        "/login/device/code",
        &[("client_id", CLI), ("scope", scopes)],
        true,
    )
    .await
}

async fn poll(router: &Router, code: &Value) -> Value {
    post(
        router,
        "/login/oauth/access_token",
        &[
            ("client_id", CLI),
            ("device_code", code["device_code"].as_str().unwrap()),
            ("grant_type", DEVICE),
        ],
        true,
    )
    .await
}

async fn approve(user: &Client, code: &Value, decision: &str) {
    let reply = user
        .form_reply(
            "/login/device",
            &[
                ("user_code", code["user_code"].as_str().unwrap()),
                ("decision", decision),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert!(
        reply.text().contains(if decision == "approve" {
            "Device authorized"
        } else {
            "Authorization denied"
        }),
        "{}",
        reply.text()
    );
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let actor = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let user = Client::login(&router, f.pool(), actor).await;
    let code = device(&router, "repo read:org gist").await;
    assert_eq!(code["interval"], 5);
    assert_eq!(code["expires_in"], 900);
    assert_eq!(code["user_code"].as_str().unwrap().len(), 9);
    assert!(code["verification_uri"]
        .as_str()
        .unwrap()
        .ends_with("/login/device"));
    assert!(
        db::oauth_apps::device_grant(f.pool(), code["user_code"].as_str().unwrap())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(poll(&router, &code).await["error"], "authorization_pending");
    let slowdown = poll(&router, &code).await;
    assert_eq!(slowdown["error"], "slow_down");
    assert_eq!(slowdown["interval"], 10);
    let reply = user
        .form_reply(
            "/login/device",
            &[("user_code", code["user_code"].as_str().unwrap())],
        )
        .await;
    assert!(reply.text().contains("GitHub CLI"));
    assert!(reply.text().contains("gist,read:org,repo"));
    let bad = user
        .exchange(
            "POST",
            "/login/device",
            "application/x-www-form-urlencoded",
            format!(
                "user_code={}&decision=approve",
                code["user_code"].as_str().unwrap()
            )
            .into_bytes(),
        )
        .await;
    assert_eq!(bad.status, StatusCode::FORBIDDEN);
    approve(&user, &code, "approve").await;
    f.execute("UPDATE oauth_app_grants SET last_poll=0", &[])
        .await;
    let (first, second) = tokio::join!(poll(&router, &code), poll(&router, &code));
    let (token, replay) = if first.get("access_token").is_some() {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(replay["error"], "incorrect_device_code");
    assert_eq!(token["token_type"], "bearer");
    let value = token["access_token"].as_str().unwrap();
    let stored = db::users::tokens::find_token(f.pool(), &token_hash(value))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.user_id, actor);
    let authorization = format!("token {value}");
    let (headers, body) = super::github::request(
        &router,
        "users/get-authenticated",
        "GET",
        "/api/v3/user",
        Some(&authorization),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["login"], "alice");
    assert_eq!(headers["x-oauth-scopes"], "gist,read:org,repo");
    let (headers, index) = super::github::request(
        &router,
        "meta/root",
        "GET",
        "/api/v3/",
        Some(&authorization),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(headers["x-oauth-scopes"], "gist,read:org,repo");
    assert!(index["current_user_url"]
        .as_str()
        .unwrap()
        .ends_with("/api/v3/user"));
    assert_eq!(poll(&router, &code).await["error"], "incorrect_device_code");
    // A deployment's automatic merge may publish workflow files, so it needs that scope too.
    let setup = codeyard::backend::identity::tokens::create(
        f.pool(),
        actor,
        Some(&user.session),
        codeyard::backend::identity::tokens::TokenInput {
            name: "setup",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let setup = format!("Bearer {}", setup.value);
    codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "deploy",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let base = "/api/v3/repos/alice/deploy";
    let (_, file) = super::github::request(
        &router,
        "repos/create-or-update-file-contents",
        "PUT",
        &format!("{base}/contents/hello.txt"),
        Some(&setup),
        json!({"message":"Initial","content":"aGVsbG8K"}),
        201,
    )
    .await;
    super::github::request(
        &router,
        "git/create-ref",
        "POST",
        &format!("{base}/git/refs"),
        Some(&setup),
        json!({"ref":"refs/heads/topic","sha":file["commit"]["sha"]}),
        201,
    )
    .await;
    super::github::request(
        &router,
        "repos/create-or-update-file-contents",
        "PUT",
        &format!("{base}/contents/other.txt"),
        Some(&setup),
        json!({"message":"Advance main","content":"bmV3Cg=="}),
        201,
    )
    .await;
    super::github::request(
        &router,
        "repos/create-deployment",
        "POST",
        &format!("{base}/deployments"),
        Some(&authorization),
        json!({"ref":"topic"}),
        403,
    )
    .await;

    let denied = device(&router, "repo").await;
    approve(&user, &denied, "deny").await;
    assert_eq!(poll(&router, &denied).await["error"], "access_denied");
    let expired = device(&router, "").await;
    f.execute(
        "UPDATE oauth_app_grants SET expires_at=1 WHERE user_id IS NULL",
        &[],
    )
    .await;
    assert_eq!(poll(&router, &expired).await["error"], "expired_token");
    assert_eq!(
        device(&router, "admin:enterprise").await["error"],
        "invalid_scope"
    );
    assert_eq!(
        post(
            &router,
            "/login/device/code",
            &[("client_id", "unknown")],
            true
        )
        .await["error"],
        "incorrect_client_credentials"
    );

    // gh's web fallback preserves its random loopback port and echoes state.
    let redirect = "http://localhost:34567/";
    let fields = [
        ("client_id", CLI),
        ("redirect_uri", redirect),
        ("scope", "repo read:org gist"),
        ("state", "browser-state"),
        ("decision", "approve"),
    ];
    let reply = user.form_reply("/login/oauth/authorize", &fields).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let redirect = url::Url::parse(reply.location.as_ref().unwrap()).unwrap();
    assert_eq!(redirect.port(), Some(34567));
    let query: std::collections::HashMap<_, _> = redirect.query_pairs().into_owned().collect();
    assert_eq!(query["state"], "browser-state");
    let web_code = query["code"].as_str();
    assert_eq!(
        post(
            &router,
            "/login/oauth/access_token",
            &[
                ("client_id", CLI),
                ("client_secret", "wrong"),
                ("code", web_code)
            ],
            true
        )
        .await["error"],
        "incorrect_client_credentials"
    );
    let web = post(
        &router,
        "/login/oauth/access_token",
        &[
            ("client_id", CLI),
            ("client_secret", SECRET),
            ("code", web_code),
            ("state", "browser-state"),
        ],
        false,
    )
    .await;
    assert!(web["access_token"].as_str().is_some());
    assert_eq!(
        post(
            &router,
            "/login/oauth/access_token",
            &[
                ("client_id", CLI),
                ("client_secret", SECRET),
                ("code", web_code)
            ],
            true
        )
        .await["error"],
        "bad_verification_code"
    );
    let reply = user
        .form_reply(
            "/login/oauth/authorize",
            &[
                ("client_id", CLI),
                ("redirect_uri", "https://attacker.test/"),
                ("decision", "approve"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.location.is_none());

    // A registered client has an exact callback and S256 proof of possession.
    let app = user
        .form_reply(
            "/settings/applications/new",
            &[
                ("name", "Example"),
                ("homepage", "https://example.test"),
                ("redirect_uri", "https://example.test/callback"),
            ],
        )
        .await;
    assert_eq!(app.status, StatusCode::OK, "{}", app.text());
    let applications = db::oauth_apps::applications(f.pool(), actor).await.unwrap();
    assert_eq!(applications.len(), 1);
    assert!(app.text().contains(&applications[0].client_id));
    let (status, page) = user.page("/settings/applications").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Example"));
    let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let reply = user
        .form_reply(
            "/login/oauth/authorize",
            &[
                ("client_id", CLI),
                ("scope", "repo"),
                ("redirect_uri", "http://localhost:45678/"),
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
                ("decision", "approve"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let url = url::Url::parse(reply.location.as_ref().unwrap()).unwrap();
    let code = url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .unwrap()
        .1
        .into_owned();
    assert_eq!(
        post(
            &router,
            "/login/oauth/access_token",
            &[
                ("client_id", CLI),
                ("client_secret", SECRET),
                ("code", &code),
                ("code_verifier", "wrong")
            ],
            true
        )
        .await["error"],
        "bad_verification_code"
    );
    assert!(post(
        &router,
        "/login/oauth/access_token",
        &[
            ("client_id", CLI),
            ("client_secret", SECRET),
            ("code", &code),
            ("code_verifier", verifier)
        ],
        true
    )
    .await["access_token"]
        .is_string());

    let readonly = device(&router, "read:org").await;
    approve(&user, &readonly, "approve").await;
    let limited = poll(&router, &readonly).await;
    let limited = db::users::tokens::find_token(
        f.pool(),
        &token_hash(limited["access_token"].as_str().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!oauth::allows_git(f.pool(), limited.id, false)
        .await
        .unwrap());
    assert!(!oauth::allows_git(f.pool(), limited.id, true).await.unwrap());
    assert_eq!(limited.scope, "read");
    let authorization = format!("Bearer {}", limited_token(&router, &user).await);
    super::github::request(
        &router,
        "repos/list-for-authenticated-user",
        "GET",
        "/api/v3/user/repos",
        Some(&authorization),
        Value::Null,
        403,
    )
    .await;
    // An application can inspect and rotate only its own OAuth tokens.
    use base64::Engine;
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{CLI}:{SECRET}"))
    );
    let path = format!("/api/v3/applications/{CLI}/token");
    let other_id = oauth::random();
    let other_app = db::oauth_apps::register(
        f.pool(),
        actor,
        &user.session,
        &other_id,
        &token_hash("other-secret"),
        "Other application",
        "https://other.test",
        "https://other.test/callback",
    )
    .await
    .unwrap()
    .unwrap();
    let other_basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{other_id}:other-secret"))
    );
    super::github::request(
        &router,
        "apps/check-token",
        "POST",
        &format!("/api/v3/applications/{other_id}/token"),
        Some(&other_basic),
        json!({"access_token":value}),
        404,
    )
    .await;
    let reply = user
        .form_reply(
            "/login/oauth/authorize",
            &[
                ("client_id", other_id.as_str()),
                ("redirect_uri", "https://other.test/callback"),
                ("scope", "repo"),
                ("decision", "approve"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let redirect = url::Url::parse(reply.location.as_ref().unwrap()).unwrap();
    let code = redirect
        .query_pairs()
        .find(|(key, _)| key == "code")
        .unwrap()
        .1
        .into_owned();
    let custom = post(
        &router,
        "/login/oauth/access_token",
        &[
            ("client_id", other_id.as_str()),
            ("client_secret", "other-secret"),
            ("code", code.as_str()),
        ],
        true,
    )
    .await;
    let custom = format!("Bearer {}", custom["access_token"].as_str().unwrap());
    super::github::request(
        &router,
        "users/get-authenticated",
        "GET",
        "/api/v3/user",
        Some(&custom),
        Value::Null,
        200,
    )
    .await;
    // Removing a client's owner cascades to the application, not to its users' token rows.
    f.execute(
        "DELETE FROM oauth_applications WHERE id=$1",
        &[other_app.id],
    )
    .await;
    super::github::request(
        &router,
        "users/get-authenticated",
        "GET",
        "/api/v3/user",
        Some(&custom),
        Value::Null,
        401,
    )
    .await;
    let (_, inspected) = super::github::request(
        &router,
        "apps/check-token",
        "POST",
        &path,
        Some(&basic),
        json!({"access_token":value}),
        200,
    )
    .await;
    assert_eq!(inspected["user"]["id"], actor);
    assert_eq!(inspected["app"]["client_id"], CLI);
    super::github::request(
        &router,
        "apps/check-token",
        "POST",
        &path,
        Some(&format!("Bearer {value}")),
        json!({"access_token":value}),
        401,
    )
    .await;
    super::github::request(
        &router,
        "apps/check-token",
        "POST",
        &path,
        Some(&basic),
        json!({}),
        422,
    )
    .await;
    super::github::request(
        &router,
        "apps/check-token",
        "POST",
        &path,
        Some(&basic),
        json!({"access_token":"unknown"}),
        404,
    )
    .await;
    let (_, rotated) = super::github::request(
        &router,
        "apps/reset-token",
        "PATCH",
        &path,
        Some(&basic),
        json!({"access_token":value}),
        200,
    )
    .await;
    let rotated = rotated["token"].as_str().unwrap();
    assert_ne!(rotated, value);
    super::github::request(
        &router,
        "apps/check-token",
        "POST",
        &path,
        Some(&basic),
        json!({"access_token":value}),
        404,
    )
    .await;
    super::github::request(
        &router,
        "apps/delete-token",
        "DELETE",
        &path,
        Some(&basic),
        json!({"access_token":rotated}),
        204,
    )
    .await;
    super::github::request(
        &router,
        "apps/delete-token",
        "DELETE",
        &path,
        Some(&basic),
        json!({"access_token":rotated}),
        204,
    )
    .await;
    super::github::request(
        &router,
        "apps/check-token",
        "POST",
        &path,
        Some(&basic),
        json!({"access_token":rotated}),
        404,
    )
    .await;
    let first = device(&router, "repo").await;
    approve(&user, &first, "approve").await;
    let first = poll(&router, &first).await;
    let second = device(&router, "repo").await;
    approve(&user, &second, "approve").await;
    let second = poll(&router, &second).await;
    super::github::request(
        &router,
        "apps/delete-authorization",
        "DELETE",
        &format!("/api/v3/applications/{CLI}/grant"),
        Some(&basic),
        json!({"access_token":first["access_token"]}),
        204,
    )
    .await;
    super::github::request(
        &router,
        "apps/check-token",
        "POST",
        &path,
        Some(&basic),
        json!({"access_token":second["access_token"]}),
        404,
    )
    .await;
    let active = device(&router, "repo").await;
    approve(&user, &active, "approve").await;
    let active = poll(&router, &active).await;
    let value = active["access_token"].as_str().unwrap();
    // Git over a repo-only token reaches repositories but not gists.
    let repo_only = db::users::tokens::find_token(f.pool(), &token_hash(value))
        .await
        .unwrap()
        .unwrap();
    assert!(oauth::allows_git(f.pool(), repo_only.id, false)
        .await
        .unwrap());
    assert!(!oauth::allows_git(f.pool(), repo_only.id, true)
        .await
        .unwrap());
    // Revoking the approving session before exchange must not mint a new credential.
    let bob = f.actor("bob").await;
    let browser = Client::login(&router, f.pool(), bob).await;
    let reply = browser
        .form_reply(
            "/login/oauth/authorize",
            &[
                ("client_id", CLI),
                ("scope", "repo"),
                ("state", "logout-race"),
                ("decision", "approve"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let redirect = url::Url::parse(reply.location.as_ref().unwrap()).unwrap();
    let code = redirect
        .query_pairs()
        .find(|(key, _)| key == "code")
        .unwrap()
        .1
        .into_owned();
    db::users::sessions::delete_session(f.pool(), &browser.session)
        .await
        .unwrap();
    assert_eq!(
        post(
            &router,
            "/login/oauth/access_token",
            &[
                ("client_id", CLI),
                ("client_secret", SECRET),
                ("code", &code)
            ],
            true
        )
        .await["error"],
        "access_denied"
    );
    // Suspending an account invalidates OAuth tokens just like personal tokens.
    f.execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[actor])
        .await;
    super::github::request(
        &router,
        "users/get-authenticated",
        "GET",
        "/api/v3/user",
        Some(&format!("Bearer {value}")),
        json!(null),
        401,
    )
    .await;
}

async fn limited_token(router: &Router, user: &Client) -> String {
    let code = device(router, "read:org").await;
    approve(user, &code, "approve").await;
    poll(router, &code).await["access_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn sqlite_oauth_apps_contract() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_oauth_apps_contract() {
    contract(true).await;
}

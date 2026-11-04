//! GitHub account documents, follow edges and keys through the real router on both engines.
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request},
    Router,
};

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use codeyard_storage::ObjectStore;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

pub(super) async fn request(
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
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status.as_u16(),
        expected,
        "{operation}: {}",
        String::from_utf8_lossy(&bytes)
    );
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        let body = serde_json::from_slice(&bytes).unwrap();
        assert_github_schema(operation, expected, &body);
        body
    };
    (headers, body)
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let carol = f.actor("carol").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[carol])
        .await;
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let session = Client::login(&router, f.pool(), alice).await;
    let other_session = Client::login(&router, f.pool(), bob).await;
    let mut credentials = Vec::new();
    let mut token_hashes = std::collections::HashMap::new();
    for (actor, session, scope) in [
        (alice, &session.session, "write"),
        (alice, &session.session, "read"),
        (bob, &other_session.session, "write"),
    ] {
        let token = tokens::create(
            f.pool(),
            actor,
            Some(session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        token_hashes.insert(
            token.token.id,
            codeyard_support::token_hash::token_hash(&token.value),
        );
        credentials.push(format!("Bearer {}", token.value));
    }
    let write = Some(credentials[0].as_str());
    let read = Some(credentials[1].as_str());
    let other = Some(credentials[2].as_str());
    for (operation, path, own) in [
        ("users/get-authenticated", "/api/v3/user".to_string(), true),
        ("users/get-by-username", "/api/v3/users/alice".into(), false),
        ("users/get-by-id", format!("/api/v3/user/{alice}"), false),
    ] {
        let (_, body) = request(&router, operation, "GET", &path, read, Value::Null, 200).await;
        assert_eq!(body["id"], alice);
        assert_eq!(body["login"], "alice");
        assert!(body["avatar_url"].as_str().unwrap().starts_with("http"));
        assert_eq!(body["followers"], 0);
        if !own {
            assert!(body["email"].is_null());
        }
        assert!(body.get("password_hash").is_none());
    }
    use codeyard_db_core::queries::organizations::OrganizationOutcome;
    use codeyard_db_orgs::organizations::profiles::NewOrganization;
    use codeyard_db_orgs::organizations::profiles::OrganizationProfile;
    let OrganizationOutcome::Applied(org) =
        codeyard_db_orgs::organizations::profiles::create_organization(
            f.pool(),
            alice,
            &NewOrganization::new(
                "acme",
                OrganizationProfile::new(Some("Acme"), Some("Tools")).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!("organization creation failed");
    };
    for token in [None, read] {
        let (_, body) = request(
            &router,
            "users/get-by-username",
            "GET",
            "/api/v3/users/acme",
            token,
            Value::Null,
            200,
        )
        .await;
        assert_eq!(body["id"], org.id);
        assert_eq!(body["login"], "acme");
        assert_eq!(body["name"], "Acme");
        assert_eq!(body["type"], "Organization");
        assert_eq!(body["site_admin"], false);
        assert!(body["node_id"].as_str().unwrap().starts_with("O_"));
        assert!(body["email"].is_null());
    }
    request(
        &router,
        "users/get-authenticated",
        "GET",
        "/api/v3/user",
        None,
        Value::Null,
        401,
    )
    .await;
    request(
        &router,
        "users/get-by-username",
        "GET",
        "/api/v3/users/nobody",
        None,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "users/get-by-id",
        "GET",
        "/api/v3/user/99999",
        None,
        Value::Null,
        404,
    )
    .await;
    let (headers, users) = request(
        &router,
        "users/list",
        "GET",
        &format!("/api/v3/users?since={}&per_page=1", alice - 1),
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(users.as_array().unwrap().len(), 1);
    assert_eq!(users[0]["id"], alice);
    assert!(headers["link"]
        .to_str()
        .unwrap()
        .contains(&format!("since={alice}")));
    let (_, users) = request(
        &router,
        "users/list",
        "GET",
        &format!("/api/v3/users?since={alice}&per_page=1"),
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(users[0]["id"], bob);

    request(
        &router,
        "users/follow",
        "PUT",
        "/api/v3/user/following/bob",
        read,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "users/follow",
        "PUT",
        "/api/v3/user/following/alice",
        write,
        Value::Null,
        422,
    )
    .await;
    request(
        &router,
        "users/follow",
        "PUT",
        "/api/v3/user/following/nobody",
        write,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "users/check-person-is-followed-by-authenticated",
        "GET",
        "/api/v3/user/following/bob",
        write,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "users/check-following-for-user",
        "GET",
        "/api/v3/users/nobody/following/alice",
        None,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "users/check-following-for-user",
        "GET",
        "/api/v3/users/alice/following/nobody",
        None,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "users/check-person-is-followed-by-authenticated",
        "GET",
        "/api/v3/user/following/nobody",
        read,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "users/unfollow",
        "DELETE",
        "/api/v3/user/following/alice",
        write,
        Value::Null,
        204,
    )
    .await;
    for name in ["bob", "bob", "carol"] {
        request(
            &router,
            "users/follow",
            "PUT",
            &format!("/api/v3/user/following/{name}"),
            write,
            Value::Null,
            204,
        )
        .await;
    }
    request(
        &router,
        "users/follow",
        "PUT",
        "/api/v3/user/following/alice",
        other,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "users/check-person-is-followed-by-authenticated",
        "GET",
        "/api/v3/user/following/bob",
        read,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "users/check-following-for-user",
        "GET",
        "/api/v3/users/alice/following/bob",
        None,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "users/check-following-for-user",
        "GET",
        "/api/v3/users/bob/following/carol",
        None,
        Value::Null,
        404,
    )
    .await;
    for (operation, path, expected) in [
        (
            "users/list-followers-for-authenticated-user",
            "/api/v3/user/followers",
            bob,
        ),
        (
            "users/list-followed-by-authenticated-user",
            "/api/v3/user/following?per_page=1",
            bob,
        ),
        (
            "users/list-followers-for-user",
            "/api/v3/users/bob/followers",
            alice,
        ),
        (
            "users/list-following-for-user",
            "/api/v3/users/bob/following",
            alice,
        ),
        (
            "users/list-following-for-user",
            "/api/v3/users/alice/following?per_page=1&page=2",
            carol,
        ),
    ] {
        let (_, body) = request(&router, operation, "GET", path, read, Value::Null, 200).await;
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["id"], expected);
        assert_eq!(body[0]["site_admin"], expected == carol);
    }
    let (_, user) = request(
        &router,
        "users/get-by-username",
        "GET",
        "/api/v3/users/alice",
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(user["followers"], 1);
    assert_eq!(user["following"], 2);
    let (_, hovercard) = request(
        &router,
        "users/get-context-for-user",
        "GET",
        "/api/v3/users/bob/hovercard",
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(hovercard["contexts"][0]["message"], "Follows you");
    request(
        &router,
        "users/get-context-for-user",
        "GET",
        "/api/v3/users/bob/hovercard?subject_type=invalid",
        read,
        Value::Null,
        422,
    )
    .await;
    for _ in 0..2 {
        request(
            &router,
            "users/unfollow",
            "DELETE",
            "/api/v3/user/following/bob",
            write,
            Value::Null,
            204,
        )
        .await;
    }
    request(
        &router,
        "users/check-following-for-user",
        "GET",
        "/api/v3/users/alice/following/bob",
        None,
        Value::Null,
        404,
    )
    .await;

    let public_key =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
    let input = json!({"title":"Laptop", "key":public_key});
    request(
        &router,
        "users/create-public-ssh-key-for-authenticated-user",
        "POST",
        "/api/v3/user/keys",
        read,
        input.clone(),
        403,
    )
    .await;
    request(
        &router,
        "users/create-public-ssh-key-for-authenticated-user",
        "POST",
        "/api/v3/user/keys",
        write,
        json!({"key":"ssh-ed25519 AAAA"}),
        422,
    )
    .await;
    let (headers, key) = request(
        &router,
        "users/create-public-ssh-key-for-authenticated-user",
        "POST",
        "/api/v3/user/keys",
        write,
        input.clone(),
        201,
    )
    .await;
    let path = format!("/api/v3/user/keys/{}", key["id"]);
    assert!(headers["location"].to_str().unwrap().ends_with(&path));
    assert_eq!(key["key"], public_key);
    assert_eq!(key["title"], "Laptop");
    request(
        &router,
        "users/create-public-ssh-key-for-authenticated-user",
        "POST",
        "/api/v3/user/keys",
        other,
        input,
        422,
    )
    .await;
    let (_, fetched) = request(
        &router,
        "users/get-public-ssh-key-for-authenticated-user",
        "GET",
        &path,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(fetched, key);
    request(
        &router,
        "users/get-public-ssh-key-for-authenticated-user",
        "GET",
        &path,
        other,
        Value::Null,
        404,
    )
    .await;
    for (op, path, auth) in [
        (
            "users/list-public-ssh-keys-for-authenticated-user",
            "/api/v3/user/keys",
            read,
        ),
        (
            "users/list-public-keys-for-user",
            "/api/v3/users/alice/keys",
            None,
        ),
    ] {
        let (_, body) = request(&router, op, "GET", path, auth, Value::Null, 200).await;
        assert_eq!(body[0]["id"], key["id"]);
        assert_eq!(body[0]["key"], public_key);
    }
    request(
        &router,
        "users/delete-public-ssh-key-for-authenticated-user",
        "DELETE",
        &path,
        other,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "users/delete-public-ssh-key-for-authenticated-user",
        "DELETE",
        &path,
        read,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "users/delete-public-ssh-key-for-authenticated-user",
        "DELETE",
        &path,
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "users/get-public-ssh-key-for-authenticated-user",
        "GET",
        &path,
        write,
        Value::Null,
        404,
    )
    .await;
    assert!(
        codeyard_db_identity::users::ssh_keys::list_user_keys(f.pool(), alice)
            .await
            .unwrap()
            .is_empty()
    );

    let signing_path = "/api/v3/user/ssh_signing_keys";
    request(
        &router,
        "users/create-ssh-signing-key-for-authenticated-user",
        "POST",
        signing_path,
        read,
        json!({"key":public_key}),
        403,
    )
    .await;
    request(
        &router,
        "users/create-ssh-signing-key-for-authenticated-user",
        "POST",
        signing_path,
        write,
        json!({"key":"ssh-ed25519 AAAA"}),
        422,
    )
    .await;
    let (headers, signing) = request(
        &router,
        "users/create-ssh-signing-key-for-authenticated-user",
        "POST",
        signing_path,
        write,
        json!({"title":"Commit signer", "key":public_key}),
        201,
    )
    .await;
    let signing_key_path = format!("{signing_path}/{}", signing["id"]);
    assert!(headers["location"]
        .to_str()
        .unwrap()
        .ends_with(&signing_key_path));
    assert_eq!(signing["key"], public_key);
    assert_eq!(signing["title"], "Commit signer");
    let (_, fetched) = request(
        &router,
        "users/get-ssh-signing-key-for-authenticated-user",
        "GET",
        &signing_key_path,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(fetched, signing);
    request(
        &router,
        "users/get-ssh-signing-key-for-authenticated-user",
        "GET",
        &signing_key_path,
        other,
        Value::Null,
        404,
    )
    .await;
    for auth in [write, other] {
        request(
            &router,
            "users/create-ssh-signing-key-for-authenticated-user",
            "POST",
            signing_path,
            auth,
            json!({"key":public_key}),
            422,
        )
        .await;
    }
    for (op, path, auth) in [
        (
            "users/list-ssh-signing-keys-for-authenticated-user",
            signing_path,
            read,
        ),
        (
            "users/list-ssh-signing-keys-for-user",
            "/api/v3/users/alice/ssh_signing_keys",
            None,
        ),
    ] {
        let (_, listed) = request(&router, op, "GET", path, auth, Value::Null, 200).await;
        assert_eq!(listed, json!([signing]));
    }
    assert!(
        codeyard_db_identity::users::ssh_keys::list_user_keys(f.pool(), alice)
            .await
            .unwrap()
            .is_empty(),
        "a signing key must not grant SSH authentication"
    );
    let page = session.page_ok("/settings?tab=keys").await;
    assert!(page.contains("SSH signing keys"));
    assert!(page.contains("Commit signer"));
    assert!(page.contains(&format!("/settings/signing-keys/{}/delete", signing["id"])));
    request(
        &router,
        "users/delete-ssh-signing-key-for-authenticated-user",
        "DELETE",
        &signing_key_path,
        read,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "users/delete-ssh-signing-key-for-authenticated-user",
        "DELETE",
        &signing_key_path,
        other,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "users/delete-ssh-signing-key-for-authenticated-user",
        "DELETE",
        &signing_key_path,
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "users/get-ssh-signing-key-for-authenticated-user",
        "GET",
        &signing_key_path,
        read,
        Value::Null,
        404,
    )
    .await;
    let response = session
        .form_reply(
            "/settings/signing-keys",
            &[("name", "Native signer"), ("public_key", public_key)],
        )
        .await;
    assert_eq!(response.status.as_u16(), 303, "{}", response.text());
    let (_, listed) = request(
        &router,
        "users/list-ssh-signing-keys-for-user",
        "GET",
        "/api/v3/users/alice/ssh_signing_keys",
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(listed[0]["title"], "Native signer");
    let response = session
        .form_reply(
            &format!("/settings/signing-keys/{}/delete", listed[0]["id"]),
            &[],
        )
        .await;
    assert_eq!(response.status.as_u16(), 303, "{}", response.text());
    let (_, listed) = request(
        &router,
        "users/list-ssh-signing-keys-for-authenticated-user",
        "GET",
        signing_path,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(listed, json!([]));

    f.execute(
        "UPDATE users SET email='alice@example.com',email_verified=TRUE WHERE id=$1",
        &[alice],
    )
    .await;
    request(
        &router,
        "users/update-authenticated",
        "PATCH",
        "/api/v3/user",
        read,
        json!({"name":"Refused"}),
        403,
    )
    .await;
    request(
        &router,
        "users/update-authenticated",
        "PATCH",
        "/api/v3/user",
        write,
        json!({"bio":"x".repeat(4001)}),
        422,
    )
    .await;
    request(
        &router,
        "users/update-authenticated",
        "PATCH",
        "/api/v3/user",
        write,
        json!({"name":"Refused", "email":"bob@example.com"}),
        422,
    )
    .await;
    let (_, updated) = request(&router, "users/update-authenticated", "PATCH", "/api/v3/user", write, json!({"name":"Alice Example", "bio":"Builder", "location":"London", "blog":"https://example.com/", "company":"Example Ltd", "twitter_username":"alice", "hireable":true, "email":"alice@example.com"}), 200).await;
    assert_eq!(updated["name"], "Alice Example");
    assert_eq!(updated["company"], "Example Ltd");
    assert_eq!(updated["hireable"], true);
    let (_, public) = request(
        &router,
        "users/get-by-username",
        "GET",
        "/api/v3/users/alice",
        None,
        Value::Null,
        200,
    )
    .await;
    let mut expected = updated.clone();
    for field in [
        "owned_private_repos",
        "total_private_repos",
        "disk_usage",
        "collaborators",
        "private_gists",
        "two_factor_authentication",
    ] {
        assert!(public.get(field).is_none(), "private field {field} leaked");
        expected.as_object_mut().unwrap().remove(field);
    }
    expected["email"] = Value::Null;
    assert_eq!(public, expected);
    let (_, cleared) = request(
        &router,
        "users/update-authenticated",
        "PATCH",
        "/api/v3/user",
        write,
        json!({"twitter_username":null, "email":null}),
        200,
    )
    .await;
    assert_eq!(cleared["twitter_username"], "");
    assert_eq!(
        cleared["bio"], "Builder",
        "omitted fields must remain unchanged"
    );
    let (_, public) = request(
        &router,
        "users/get-by-username",
        "GET",
        "/api/v3/users/alice",
        None,
        Value::Null,
        200,
    )
    .await;
    assert!(public["email"].is_null());
    let form = session
        .form_reply(
            "/settings/profile",
            &[
                ("display_name", "Alice"),
                ("bio", "Builder"),
                ("website", "https://example.com/"),
                ("location", "London"),
                ("company", "Native Company"),
                ("twitter_username", "alice-native"),
                ("hireable", "yes"),
                ("profile_details", "yes"),
            ],
        )
        .await;
    assert_eq!(form.status.as_u16(), 303, "{}", form.text());
    let (_, public) = request(
        &router,
        "users/get-by-username",
        "GET",
        "/api/v3/users/alice",
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(public["company"], "Native Company");
    assert_eq!(public["twitter_username"], "alice-native");
    let page = session.page_ok("/alice").await;
    assert!(page.contains("Native Company"));
    assert!(page.contains("Available for hire"));

    let (_, emails) = request(
        &router,
        "users/list-emails-for-authenticated-user",
        "GET",
        "/api/v3/user/emails",
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(
        emails,
        json!([{"email":"alice@example.com","primary":true,"verified":true,"visibility":"private"}])
    );
    let (_, emails) = request(
        &router,
        "users/list-public-emails-for-authenticated-user",
        "GET",
        "/api/v3/user/public_emails",
        write,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(emails, json!([]));
    request(
        &router,
        "users/set-primary-email-visibility-for-authenticated-user",
        "PATCH",
        "/api/v3/user/email/visibility",
        read,
        json!({"visibility":"public"}),
        403,
    )
    .await;
    request(
        &router,
        "users/set-primary-email-visibility-for-authenticated-user",
        "PATCH",
        "/api/v3/user/email/visibility",
        write,
        json!({"visibility":"unknown"}),
        422,
    )
    .await;
    let (_, emails) = request(
        &router,
        "users/set-primary-email-visibility-for-authenticated-user",
        "PATCH",
        "/api/v3/user/email/visibility",
        write,
        json!({"visibility":"public"}),
        200,
    )
    .await;
    assert_eq!(emails[0]["visibility"], "public");
    let (_, emails) = request(
        &router,
        "users/list-public-emails-for-authenticated-user",
        "GET",
        "/api/v3/user/public_emails",
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(emails.as_array().unwrap().len(), 1);
    f.execute(
        "UPDATE users SET email_verified=FALSE WHERE id=$1",
        &[alice],
    )
    .await;
    let (_, emails) = request(
        &router,
        "users/list-public-emails-for-authenticated-user",
        "GET",
        "/api/v3/user/public_emails",
        read,
        Value::Null,
        200,
    )
    .await;
    assert!(emails.as_array().unwrap().is_empty());
    let (_, user) = request(
        &router,
        "users/get-by-username",
        "GET",
        "/api/v3/users/alice",
        None,
        Value::Null,
        200,
    )
    .await;
    assert!(user["email"].is_null());
    f.execute("UPDATE users SET email_verified=TRUE WHERE id=$1", &[alice])
        .await;
    let page = session.page_ok("/settings?tab=email").await;
    assert!(page.contains("/settings/email-visibility"));
    let response = session.form_reply("/settings/email-visibility", &[]).await;
    assert_eq!(response.status.as_u16(), 303, "{}", response.text());
    let (_, emails) = request(
        &router,
        "users/list-public-emails-for-authenticated-user",
        "GET",
        "/api/v3/user/public_emails",
        read,
        Value::Null,
        200,
    )
    .await;
    assert!(emails.as_array().unwrap().is_empty());

    let social_path = "/api/v3/user/social_accounts";
    request(
        &router,
        "users/add-social-account-for-authenticated-user",
        "POST",
        social_path,
        read,
        json!({"account_urls":["https://x.com/alice"]}),
        403,
    )
    .await;
    for url in [
        "javascript:alert(1)",
        "https://user:password@example.com/",
        "not a URL",
    ] {
        request(
            &router,
            "users/add-social-account-for-authenticated-user",
            "POST",
            social_path,
            write,
            json!({"account_urls":[url]}),
            422,
        )
        .await;
    }
    let (_, accounts) = request(
        &router,
        "users/add-social-account-for-authenticated-user",
        "POST",
        social_path,
        write,
        json!({"account_urls":["https://x.com/alice", "https://example.com/alice"]}),
        201,
    )
    .await;
    assert_eq!(accounts.as_array().unwrap().len(), 2);
    assert_eq!(accounts[1]["provider"], "twitter");
    request(
        &router,
        "users/add-social-account-for-authenticated-user",
        "POST",
        social_path,
        write,
        json!({"account_urls":["https://x.com/alice"]}),
        201,
    )
    .await;
    for (operation, path) in [
        (
            "users/list-social-accounts-for-authenticated-user",
            social_path,
        ),
        (
            "users/list-social-accounts-for-user",
            "/api/v3/users/alice/social_accounts",
        ),
    ] {
        let (_, fetched) = request(&router, operation, "GET", path, read, Value::Null, 200).await;
        assert_eq!(fetched, accounts);
    }
    request(&router, "users/add-social-account-for-authenticated-user", "POST", social_path, write, json!({"account_urls":["https://one.example/alice", "https://two.example/alice", "https://three.example/alice"]}), 422).await;
    let (_, fetched) = request(
        &router,
        "users/list-social-accounts-for-authenticated-user",
        "GET",
        social_path,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(fetched, accounts, "a rejected batch must be rolled back");
    request(
        &router,
        "users/delete-social-account-for-authenticated-user",
        "DELETE",
        social_path,
        other,
        json!({"account_urls":["https://x.com/alice"]}),
        204,
    )
    .await;
    let (_, fetched) = request(
        &router,
        "users/list-social-accounts-for-authenticated-user",
        "GET",
        social_path,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(
        fetched, accounts,
        "another account cannot remove these links"
    );
    let page = session.page_ok("/settings?tab=profile").await;
    assert!(page.contains("Social accounts"));
    assert!(page.contains("https://x.com/alice"));
    let response = session
        .form_reply(
            "/settings/social-accounts",
            &[("url", "https://linkedin.com/in/alice")],
        )
        .await;
    assert_eq!(response.status.as_u16(), 303, "{}", response.text());
    let (_, accounts) = request(
        &router,
        "users/list-social-accounts-for-user",
        "GET",
        "/api/v3/users/alice/social_accounts",
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(accounts.as_array().unwrap().len(), 3);
    let page = session.page_ok("/alice").await;
    assert!(page.contains("https://linkedin.com/in/alice"));
    let response = session
        .form_reply(
            "/settings/social-accounts",
            &[("url", "https://linkedin.com/in/alice"), ("remove", "yes")],
        )
        .await;
    assert_eq!(response.status.as_u16(), 303, "{}", response.text());
    request(
        &router,
        "users/delete-social-account-for-authenticated-user",
        "DELETE",
        social_path,
        read,
        json!({"account_urls":["https://x.com/alice"]}),
        403,
    )
    .await;
    request(
        &router,
        "users/delete-social-account-for-authenticated-user",
        "DELETE",
        social_path,
        write,
        json!({"account_urls":["https://x.com/alice"]}),
        204,
    )
    .await;
    let (_, accounts) = request(
        &router,
        "users/list-social-accounts-for-user",
        "GET",
        "/api/v3/users/alice/social_accounts",
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(accounts.as_array().unwrap().len(), 1);

    let mut repositories = Vec::new();
    for owner in [alice, bob] {
        let repo = codeyard_app::repositories::lifecycle::provision(
            (f.pool(), None),
            root.path(),
            codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner),
            "private",
            None,
            true,
            None,
        )
        .await
        .unwrap();
        repositories.push(repo);
    }
    f.execute(
        "UPDATE repositories SET size_bytes=4096 WHERE id=$1",
        &[repositories[0].id],
    )
    .await;
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repositories[0].id,
        codeyard_db_core::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        alice,
        repositories[1].id,
        codeyard_db_core::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let (_, user) = request(
        &router,
        "users/get-authenticated",
        "GET",
        "/api/v3/user",
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(user["owned_private_repos"], 1);
    assert_eq!(user["total_private_repos"], 2);
    assert_eq!(user["disk_usage"], 4);
    assert_eq!(user["collaborators"], 1);
    assert_eq!(user["two_factor_authentication"], false);
    let (_, public) = request(
        &router,
        "users/get-by-username",
        "GET",
        "/api/v3/users/alice",
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(public["public_repos"], 0);
    assert!(public.get("total_private_repos").is_none());
    let credentials = codeyard_db_identity::users::tokens::list_tokens(
        f.pool(),
        alice,
        &crate::support::live_session(f.pool(), alice).await,
        0,
    )
    .await
    .unwrap();
    let read_token = credentials
        .iter()
        .find(|token| token.scope == "read")
        .unwrap();
    let read_hash = &token_hashes[&read_token.id];
    let patch = codeyard_db_identity::users::profiles::ProfilePatch {
        company: Some("Must not be saved"),
        ..codeyard_db_identity::users::profiles::ProfilePatch::default()
    };
    let denied = codeyard_identity::profiles::save(
        f.pool(),
        alice,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Token(
            read_token.id,
            read_hash,
        ),
        &patch,
    )
    .await
    .unwrap_err();
    assert_eq!(denied, codeyard_identity::profiles::ProfileError::Denied);
    let write_token = credentials
        .iter()
        .find(|token| token.scope == "write")
        .unwrap();
    let write_hash = &token_hashes[&write_token.id];
    // Hash mismatch is distinct from read-only scope and revocation: this write token is live.
    let denied = codeyard_identity::profiles::save(
        f.pool(),
        alice,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Token(
            write_token.id,
            read_hash,
        ),
        &patch,
    )
    .await
    .unwrap_err();
    assert_eq!(denied, codeyard_identity::profiles::ProfileError::Denied);
    f.execute(
        "DELETE FROM personal_access_tokens WHERE id=$1",
        &[write_token.id],
    )
    .await;
    let denied = codeyard_identity::profiles::save(
        f.pool(),
        alice,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Token(
            write_token.id,
            write_hash,
        ),
        &patch,
    )
    .await
    .unwrap_err();
    assert_eq!(denied, codeyard_identity::profiles::ProfileError::Denied);
    assert_eq!(
        codeyard_db_identity::users::profiles::get_profile_settings(f.pool(), alice)
            .await
            .unwrap()
            .company
            .as_deref(),
        Some("Native Company")
    );
    for (token, hash) in [
        (read_token.id, read_hash.as_str()),
        (write_token.id, write_hash.as_str()),
    ] {
        let error = codeyard_db_identity::users::ssh_keys::add_ssh_key_for_token(
            f.pool(),
            alice,
            token,
            hash,
            "Must not be added",
            public_key,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<codeyard_model::identity::SshKeyWriteRefusal>(),
            Some(&codeyard_model::identity::SshKeyWriteRefusal::AccountRestricted)
        );
        let error = codeyard_identity::keys::enroll_signing(
            f.pool(),
            alice,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Token(token, hash),
            "Must not be added",
            public_key,
        )
        .await
        .unwrap_err();
        assert_eq!(error, codeyard_identity::keys::KeyError::AccountRestricted);
    }
    let key =
        codeyard_db_identity::users::ssh_keys::add_ssh_key(f.pool(), alice, "Kept", public_key)
            .await
            .unwrap();
    let error = codeyard_db_identity::users::ssh_keys::delete_user_ssh_key_for_token(
        f.pool(),
        alice,
        write_token.id,
        write_hash,
        key.id,
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<codeyard_model::identity::SshKeyWriteRefusal>(),
        Some(&codeyard_model::identity::SshKeyWriteRefusal::AccountRestricted)
    );
    assert_eq!(
        codeyard_db_identity::users::ssh_keys::list_user_keys(f.pool(), alice)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn sqlite_github_users() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_users() {
    contract(true).await;
}

use super::applied;
use super::request;
use super::Client;
use super::Database;
use super::OrgRole;
use super::TokenInput;
use axum::http::StatusCode;
use base64::{engine::general_purpose::STANDARD, Engine};
use codeyard_db_orgs::organizations::profiles::NewOrganization;
use codeyard_db_orgs::organizations::profiles::OrganizationProfile;
use codeyard_identity::tokens;

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;
use codeyard_storage::ObjectStore;
use codeyard_support::secrets::SecretEncryptor;
use serde_json::{json, Value};
use std::sync::Arc;

const BASE: &str = "/api/v3/orgs/acme/private-registries";

fn sealed(key: &Value, secret: &[u8]) -> Value {
    let bytes: [u8; 32] = STANDARD
        .decode(key["key"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let public = crypto_box::PublicKey::from(bytes);
    json!({"key_id":key["key_id"], "encrypted_value":STANDARD.encode(public.seal(&mut rand::thread_rng(),secret).unwrap())})
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let mut orgs = Vec::new();
    for name in ["acme", "second"] {
        orgs.push(applied(
            codeyard_db_orgs::organizations::profiles::create_organization(
                f.pool(),
                owner,
                &NewOrganization::new(name, OrganizationProfile::new(None, None).unwrap()).unwrap(),
            )
            .await
            .unwrap(),
        ));
    }
    applied(
        codeyard_db_orgs::organizations::membership::add_organization_member(
            f.pool(),
            owner,
            orgs[0].id,
            member,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (org, name) in [(orgs[0].id, "one"), (orgs[1].id, "foreign")] {
        repos.push(
            codeyard_repos_core::repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                codeyard_db_repos::repositories::creation::RepositoryCreationOwner {
                    actor_id: owner,
                    target:
                        codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(
                            org,
                        ),
                },
                name,
                None,
                true,
                None,
            )
            .await
            .unwrap(),
        );
    }
    let encryptor = Arc::new(SecretEncryptor::new(&[0x52; 32]));
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(encryptor.clone()),
        CiConfig::default(),
        None,
    ));
    let session = Client::login(&router, f.pool(), owner).await;
    let member_session = Client::login(&router, f.pool(), member).await;
    let mut credentials = Vec::new();
    for (actor, session, scope) in [
        (owner, &session, "write"),
        (owner, &session, "read"),
        (member, &member_session, "write"),
    ] {
        let token = tokens::create(
            f.pool(),
            actor,
            Some(&session.session),
            TokenInput {
                name: "registries",
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let writer = Some(credentials[0].as_str());
    let reader = Some(credentials[1].as_str());
    let (_, key) = request(
        &router,
        "private-registries/get-org-public-key",
        "GET",
        &format!("{BASE}/public-key"),
        reader,
        Value::Null,
        200,
    )
    .await;
    let (_, again) = request(
        &router,
        "private-registries/get-org-public-key",
        "GET",
        &format!("{BASE}/public-key"),
        reader,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(key, again);
    let mut input = sealed(&key, b"registry-only-password");
    for (key,value) in json!({"registry_type":"npm_registry","url":"https://registry.example.test/npm/","username":"robot","visibility":"selected","selected_repository_ids":[repos[0].id]}).as_object().unwrap() { input[key] = value.clone(); }
    let (_, registry) = request(
        &router,
        "private-registries/create-org-private-registry",
        "POST",
        BASE,
        writer,
        input.clone(),
        201,
    )
    .await;
    let name = registry["name"].as_str().unwrap();
    let path = format!("{BASE}/{name}");
    assert_eq!(registry["selected_repository_ids"], json!([repos[0].id]));
    assert_eq!(registry["auth_type"], "token");
    assert!(!registry.to_string().contains("encrypted_value"));
    let (_, fetched) = request(
        &router,
        "private-registries/get-org-private-registry",
        "GET",
        &path,
        reader,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(fetched, registry);
    let (encrypted, nonce): (Vec<u8>, Vec<u8>) = match f.pool() {
        codeyard_db_core::DbPool::Sqlite(pool) => sqlx::query_as(
            "SELECT encrypted_value,nonce FROM org_private_registries WHERE org_id=$1 AND name=$2",
        )
        .bind(orgs[0].id)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap(),
        codeyard_db_core::DbPool::Postgres(pool) => sqlx::query_as(
            "SELECT encrypted_value,nonce FROM org_private_registries WHERE org_id=$1 AND name=$2",
        )
        .bind(orgs[0].id)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap(),
    };
    assert_ne!(encrypted, b"registry-only-password");
    assert_eq!(
        encryptor.decrypt(&encrypted, &nonce).unwrap(),
        b"registry-only-password"
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM org_dependabot_secrets WHERE org_id=$1",
            &[orgs[0].id]
        )
        .await,
        0
    );
    request(
        &router,
        "private-registries/update-org-private-registry",
        "PATCH",
        &path,
        writer,
        json!({"username":"changed","visibility":"private","replaces_base":true}),
        204,
    )
    .await;
    let (_, fetched) = request(
        &router,
        "private-registries/get-org-private-registry",
        "GET",
        &path,
        reader,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(fetched["username"], "changed");
    assert_eq!(fetched["visibility"], "private");
    assert_eq!(fetched["created_at"], registry["created_at"]);
    assert!(fetched.get("selected_repository_ids").is_none());
    for body in [
        json!({"auth_type":"anonymous"}),
        json!({"registry_type":"unknown"}),
        json!({"url":"https://user:password@example.test"}),
        json!({"visibility":"selected","selected_repository_ids":[repos[1].id]}),
        json!({"encrypted_value":"bad"}),
    ] {
        request(
            &router,
            "private-registries/update-org-private-registry",
            "PATCH",
            &path,
            writer,
            body,
            422,
        )
        .await;
    }
    let mut bad = input.clone();
    bad["key_id"] = json!("wrong-key");
    request(
        &router,
        "private-registries/create-org-private-registry",
        "POST",
        BASE,
        writer,
        bad,
        422,
    )
    .await;
    let mut bad = input.clone();
    bad["selected_repository_ids"] = json!([repos[1].id]);
    request(
        &router,
        "private-registries/create-org-private-registry",
        "POST",
        BASE,
        writer,
        bad,
        422,
    )
    .await;
    for (auth, fields) in [
        ("anonymous", json!({})),
        (
            "oidc_azure",
            json!({"tenant_id":"tenant","client_id":"client"}),
        ),
        (
            "oidc_aws",
            json!({"aws_region":"region","account_id":"account","role_name":"role","domain":"domain","domain_owner":"owner"}),
        ),
        ("oidc_jfrog", json!({"jfrog_oidc_provider_name":"provider"})),
        (
            "oidc_cloudsmith",
            json!({"namespace":"org","service_slug":"service","audience":"https://example.test"}),
        ),
        (
            "oidc_gcp",
            json!({"workload_identity_provider":"projects/1/locations/global/workloadIdentityPools/p/providers/p"}),
        ),
    ] {
        let mut body = json!({"registry_type":"docker_registry","url":"https://registry.example.test","auth_type":auth,"visibility":"all"});
        if auth != "anonymous" {
            request(
                &router,
                "private-registries/create-org-private-registry",
                "POST",
                BASE,
                writer,
                body.clone(),
                422,
            )
            .await;
        }
        for (key, value) in fields.as_object().unwrap() {
            body[key] = value.clone();
        }
        request(
            &router,
            "private-registries/create-org-private-registry",
            "POST",
            BASE,
            writer,
            body,
            201,
        )
        .await;
    }
    let (headers, list) = request(
        &router,
        "private-registries/list-org-private-registries",
        "GET",
        &format!("{BASE}?per_page=1"),
        reader,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list["total_count"], 7);
    assert_eq!(list["configurations"].as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (_, list) = request(
        &router,
        "private-registries/list-org-private-registries",
        "GET",
        &format!("{BASE}?per_page=1&page=8"),
        reader,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list["configurations"], json!([]));
    for (token, status) in [(None, 401), (Some(credentials[2].as_str()), 403)] {
        for (operation, suffix) in [
            ("private-registries/list-org-private-registries", ""),
            ("private-registries/get-org-private-registry", name),
            ("private-registries/get-org-public-key", "public-key"),
        ] {
            let path = if suffix.is_empty() {
                BASE.to_owned()
            } else {
                format!("{BASE}/{suffix}")
            };
            request(&router, operation, "GET", &path, token, Value::Null, status).await;
        }
        request(
            &router,
            "private-registries/create-org-private-registry",
            "POST",
            BASE,
            token,
            input.clone(),
            status,
        )
        .await;
        request(
            &router,
            "private-registries/update-org-private-registry",
            "PATCH",
            &path,
            token,
            json!({}),
            status,
        )
        .await;
        request(
            &router,
            "private-registries/delete-org-private-registry",
            "DELETE",
            &path,
            token,
            Value::Null,
            status,
        )
        .await;
    }
    request(
        &router,
        "private-registries/create-org-private-registry",
        "POST",
        BASE,
        reader,
        input,
        403,
    )
    .await;
    request(
        &router,
        "private-registries/update-org-private-registry",
        "PATCH",
        &path,
        reader,
        json!({}),
        403,
    )
    .await;
    request(
        &router,
        "private-registries/delete-org-private-registry",
        "DELETE",
        &path,
        reader,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "private-registries/get-org-private-registry",
        "GET",
        &format!("/api/v3/orgs/second/private-registries/{name}"),
        reader,
        Value::Null,
        404,
    )
    .await;
    let settings = format!(
        "/settings/organizations/{}?tab=private-registries",
        orgs[0].id
    );
    let target = format!(
        "/settings/organizations/{}/private-registries?tab=private-registries",
        orgs[0].id
    );
    let html = session.page_ok(&settings).await;
    assert!(html.contains("Private registries"));
    assert!(!html.contains("registry-only-password"));
    let (_, model) = session.model(&settings).await;
    assert!(!serde_json::to_string(&model)
        .unwrap()
        .contains("registry-only-password"));
    let reply = session
        .form_reply(
            &target,
            &[
                ("action", "registry-save"),
                ("name", name),
                ("registry_type", "npm_registry"),
                ("url", "https://registry.example.test/native"),
                ("auth_type", "token"),
                ("username", "native-user"),
                ("visibility", "private"),
                ("replaces_base", "false"),
                ("value", "native-password"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let (_, fetched) = request(
        &router,
        "private-registries/get-org-private-registry",
        "GET",
        &path,
        reader,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(fetched["username"], "native-user");
    let reply = session
        .form_reply(
            &target,
            &[
                ("action", "registry-save"),
                ("name", name),
                ("registry_type", "npm_registry"),
                ("url", "bad-url"),
                ("auth_type", "token"),
                ("visibility", "private"),
                ("replaces_base", "false"),
                ("value", "never-reflect-this"),
            ],
        )
        .await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.text()
    );
    assert!(!reply.text().contains("never-reflect-this"));
    assert!(reply.text().contains("bad-url"));
    let reply = session
        .exchange(
            "POST",
            &target,
            "application/x-www-form-urlencoded",
            b"action=registry-delete".to_vec(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = member_session
        .form_reply(&target, &[("action", "registry-delete"), ("name", name)])
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    request(
        &router,
        "private-registries/delete-org-private-registry",
        "DELETE",
        &path,
        writer,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "private-registries/get-org-private-registry",
        "GET",
        &path,
        reader,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "private-registries/update-org-private-registry",
        "PATCH",
        &path,
        writer,
        json!({}),
        404,
    )
    .await;
    request(
        &router,
        "private-registries/delete-org-private-registry",
        "DELETE",
        &path,
        writer,
        Value::Null,
        404,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn sqlite_private_registries() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_private_registries() {
    contract(true).await;
}

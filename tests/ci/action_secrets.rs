//! Repository secrets use sealed-box input, encrypted storage and metadata-only reads.
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use codeyard_security::secrets::SecretEncryptor;
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
    input: Value,
    expected: u16,
) -> (HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(
            request
                .body(if input.is_null() {
                    Body::empty()
                } else {
                    Body::from(input.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        status.as_u16(),
        expected,
        "{method} {path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    let body = if status == StatusCode::NO_CONTENT {
        assert!(bytes.is_empty());
        Value::Null
    } else {
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_github_schema(operation, expected, &body);
        body
    };
    (headers, body)
}

pub(super) async fn gh_list(
    router: &Router,
    operation: &str,
    base: &str,
    reader: &str,
    plural: &str,
) -> Value {
    let (_, list) = request(
        router,
        operation,
        "GET",
        &format!("{base}?per_page=100"),
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    let rows = list[plural].as_array().unwrap();
    assert_eq!(list["total_count"].as_u64().unwrap() as usize, rows.len());
    for row in rows {
        assert!(!row["name"].as_str().unwrap().is_empty());
        chrono::DateTime::parse_from_rfc3339(row["updated_at"].as_str().unwrap()).unwrap();
        if plural == "variables" {
            assert!(row["value"].is_string());
            chrono::DateTime::parse_from_rfc3339(row["created_at"].as_str().unwrap()).unwrap();
        }
        if base.starts_with("/api/v3/orgs/") {
            assert!(matches!(
                row["visibility"].as_str().unwrap(),
                "all" | "private" | "selected"
            ));
            if row["visibility"] == "selected" {
                let url = row["selected_repositories_url"].as_str().unwrap();
                let url = url::Url::parse(url).unwrap();
                let noun = plural.trim_end_matches('s');
                let operation = operation.replace(
                    &format!("list-org-{plural}"),
                    &format!("list-selected-repos-for-org-{noun}"),
                );
                let (_, selected) = request(
                    router,
                    &operation,
                    "GET",
                    url.path(),
                    Some(reader),
                    Value::Null,
                    200,
                )
                .await;
                assert_eq!(
                    selected["total_count"].as_u64().unwrap() as usize,
                    selected["repositories"].as_array().unwrap().len()
                );
            }
        }
    }
    list
}

pub(super) async fn pat(f: &Database, router: &Router, actor: i64, scope: &str) -> String {
    let client = Client::login(router, f.pool(), actor).await;
    tokens::create(
        f.pool(),
        actor,
        Some(&client.session),
        TokenInput {
            name: "Actions secrets",
            scope,
            expires_at: None,
        },
    )
    .await
    .unwrap()
    .value
}

pub(super) fn sealed(key: &Value, value: &[u8]) -> Value {
    let bytes: [u8; 32] = STANDARD
        .decode(key["key"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let key_bytes = crypto_box::PublicKey::from(bytes);
    json!({"key_id":key["key_id"], "encrypted_value":STANDARD.encode(key_bytes.seal(&mut crypto_box::aead::OsRng, value).unwrap())})
}

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("alice").await;
    let outsider = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_git_storage::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner),
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let second = codeyard_git_storage::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner),
        "other",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let encryptor = Arc::new(SecretEncryptor::new(&[0x63; 32]));
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(encryptor.clone()),
        CiConfig::default(),
        None,
    ));
    let writer = pat(&f, &router, owner, "write").await;
    let reader = pat(&f, &router, owner, "read").await;
    let stranger = pat(&f, &router, outsider, "write").await;
    let base = "/api/v3/repos/alice/project/actions/secrets";
    let (_, key) = request(
        &router,
        "actions/get-repo-public-key",
        "GET",
        &format!("{base}/public-key"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    let (_, again) = request(
        &router,
        "actions/get-repo-public-key",
        "GET",
        &format!("{base}/public-key"),
        Some(&writer),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(key, again);
    let (_, other_key) = request(
        &router,
        "actions/get-repo-public-key",
        "GET",
        "/api/v3/repos/alice/other/actions/secrets/public-key",
        Some(&writer),
        Value::Null,
        200,
    )
    .await;
    assert_ne!(key, other_key);
    let value = b"a secret with\nnewlines and quotes \" '$";
    let input = sealed(&key, value);
    request(
        &router,
        "actions/create-or-update-repo-secret",
        "PUT",
        &format!("{base}/deploy_token"),
        Some(&writer),
        input.clone(),
        201,
    )
    .await;
    request(
        &router,
        "actions/create-or-update-repo-secret",
        "PUT",
        &format!("{base}/DEPLOY_TOKEN"),
        Some(&writer),
        input.clone(),
        204,
    )
    .await;
    request(
        &router,
        "actions/create-or-update-repo-secret",
        "PUT",
        &format!("{base}/SECOND"),
        Some(&writer),
        sealed(&key, b"second"),
        201,
    )
    .await;
    let (headers, list) = request(
        &router,
        "actions/list-repo-secrets",
        "GET",
        &format!("{base}?per_page=1"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list["total_count"], 2);
    assert_eq!(list["secrets"][0]["name"], "DEPLOY_TOKEN");
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    assert!(list["secrets"][0].get("value").is_none());
    let (_, page) = request(
        &router,
        "actions/list-repo-secrets",
        "GET",
        &format!("{base}?per_page=1&page=2"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(page["secrets"][0]["name"], "SECOND");
    let (_, meta) = request(
        &router,
        "actions/get-repo-secret",
        "GET",
        &format!("{base}/deploy_token"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(meta, list["secrets"][0]);
    let gh_list = gh_list(
        &router,
        "actions/list-repo-secrets",
        base,
        &reader,
        "secrets",
    )
    .await;
    assert_eq!(gh_list["secrets"][0], meta);
    assert_eq!(gh_list["secrets"][1], page["secrets"][0]);
    let delivered =
        codeyard_db_actions::secrets::decrypt_repository_secrets(f.pool(), &encryptor, repo.id)
            .await
            .unwrap();
    assert!(delivered.contains(&(
        "DEPLOY_TOKEN".into(),
        String::from_utf8(value.to_vec()).unwrap()
    )));
    assert!(
        !codeyard_db_actions::secrets::repository_has_secrets(f.pool(), second.id)
            .await
            .unwrap()
    );
    for (method, path, operation, token, status, input) in [
        (
            "GET",
            base.to_owned(),
            "actions/list-repo-secrets",
            None,
            404,
            Value::Null,
        ),
        (
            "GET",
            base.to_owned(),
            "actions/list-repo-secrets",
            Some(stranger.as_str()),
            404,
            Value::Null,
        ),
        (
            "GET",
            format!("{base}/MISSING"),
            "actions/get-repo-secret",
            Some(reader.as_str()),
            404,
            Value::Null,
        ),
        (
            "PUT",
            format!("{base}/NO_WRITE"),
            "actions/create-or-update-repo-secret",
            Some(reader.as_str()),
            403,
            input.clone(),
        ),
        (
            "DELETE",
            format!("{base}/SECOND"),
            "actions/delete-repo-secret",
            Some(reader.as_str()),
            403,
            Value::Null,
        ),
        (
            "PUT",
            format!("{base}/GITHUB_TOKEN"),
            "actions/create-or-update-repo-secret",
            Some(writer.as_str()),
            422,
            input.clone(),
        ),
        (
            "PUT",
            format!("{base}/BAD-NAME"),
            "actions/create-or-update-repo-secret",
            Some(writer.as_str()),
            422,
            input.clone(),
        ),
        (
            "PUT",
            format!("{base}/BAD_KEY"),
            "actions/create-or-update-repo-secret",
            Some(writer.as_str()),
            422,
            sealed(&other_key, b"wrong repository"),
        ),
        (
            "PUT",
            format!("{base}/BAD_BOX"),
            "actions/create-or-update-repo-secret",
            Some(writer.as_str()),
            422,
            json!({"key_id":key["key_id"], "encrypted_value":"not base64"}),
        ),
        (
            "PUT",
            format!("{base}/BAD_BOX"),
            "actions/create-or-update-repo-secret",
            Some(writer.as_str()),
            422,
            json!({"key_id":key["key_id"], "encrypted_value":sealed(&other_key, b"wrong key")["encrypted_value"]}),
        ),
        (
            "PUT",
            format!("{base}/NUL"),
            "actions/create-or-update-repo-secret",
            Some(writer.as_str()),
            422,
            sealed(&key, b"no\0nul"),
        ),
        (
            "PUT",
            format!("{base}/EMPTY_INPUT"),
            "actions/create-or-update-repo-secret",
            Some(writer.as_str()),
            422,
            json!({}),
        ),
    ] {
        request(&router, operation, method, &path, token, input, status).await;
    }
    request(
        &router,
        "actions/create-or-update-repo-secret",
        "PUT",
        &format!("{base}/TOO_LARGE"),
        Some(&writer),
        sealed(&key, &vec![b'x'; 49153]),
        422,
    )
    .await;
    request(
        &router,
        "actions/delete-repo-secret",
        "DELETE",
        &format!("{base}/SECOND"),
        Some(&writer),
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "actions/delete-repo-secret",
        "DELETE",
        &format!("{base}/SECOND"),
        Some(&writer),
        Value::Null,
        404,
    )
    .await;
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        outsider,
        repo.id,
        codeyard_database::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    for (path, operation) in [
        (base.to_owned(), "actions/list-repo-secrets"),
        (format!("{base}/public-key"), "actions/get-repo-public-key"),
        (format!("{base}/DEPLOY_TOKEN"), "actions/get-repo-secret"),
    ] {
        request(
            &router,
            operation,
            "GET",
            &path,
            Some(&stranger),
            Value::Null,
            403,
        )
        .await;
    }
    request(
        &router,
        "actions/create-or-update-repo-secret",
        "PUT",
        &format!("{base}/NO_ADMIN"),
        Some(&stranger),
        input.clone(),
        403,
    )
    .await;
    let stored: Vec<u8> = match f.pool() {
        codeyard_database::DbPool::Sqlite(pool) => sqlx::query_scalar(
            "SELECT encrypted_value FROM repo_secrets WHERE repo_id=$1 AND name='DEPLOY_TOKEN'",
        )
        .bind(repo.id)
        .fetch_one(pool)
        .await
        .unwrap(),
        codeyard_database::DbPool::Postgres(pool) => sqlx::query_scalar(
            "SELECT encrypted_value FROM repo_secrets WHERE repo_id=$1 AND name='DEPLOY_TOKEN'",
        )
        .bind(repo.id)
        .fetch_one(pool)
        .await
        .unwrap(),
    };
    assert_ne!(stored, value);
    assert_ne!(
        stored,
        STANDARD
            .decode(input["encrypted_value"].as_str().unwrap())
            .unwrap()
    );
    let client = Client::login(&router, f.pool(), owner).await;
    let page = client.page_ok("/alice/project/settings?tab=secrets").await;
    assert!(page.contains("DEPLOY_TOKEN"));
    assert!(!page.contains(std::str::from_utf8(value).unwrap()));
    let response = client
        .form_reply(
            "/alice/project/_forms/secret-save",
            &[
                ("repository_id", repo.id.to_string().as_str()),
                ("name", "FROM_FORM"),
                ("value", "native-form-secret"),
            ],
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "{}",
        response.text()
    );
    let response = client
        .form_reply(
            "/alice/project/_forms/secret-save",
            &[
                ("repository_id", repo.id.to_string().as_str()),
                ("name", "BAD-NAME"),
                ("value", "never-echo-this-secret"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(!response.text().contains("never-echo-this-secret"));
    f.finish().await;
}

#[tokio::test]
async fn repository_secrets_sqlite() {
    contract(false).await;
}

#[tokio::test]
async fn repository_secrets_postgres() {
    contract(true).await;
}

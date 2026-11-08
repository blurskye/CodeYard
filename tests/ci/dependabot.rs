//! Dependabot secrets must never be exposed to an Actions run.
use super::action_secrets::{pat, request, sealed};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;
use codeyard_git_storage::repositories::lifecycle::provision;
use codeyard_security::secrets::SecretEncryptor;
use codeyard_storage::ObjectStore;
use serde_json::{json, Value};
use std::sync::Arc;

async fn repository_secrets(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("alice").await;
    let outsider = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = provision(
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
    let encryptor = Arc::new(SecretEncryptor::new(&[0x64; 32]));
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
    let base = "/api/v3/repos/alice/project/dependabot/secrets";
    let (_, key) = request(
        &router,
        "dependabot/get-repo-public-key",
        "GET",
        &format!("{base}/public-key"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    let (_, again) = request(
        &router,
        "dependabot/get-repo-public-key",
        "GET",
        &format!("{base}/public-key"),
        Some(&writer),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(key, again);
    let (_, actions_key) = request(
        &router,
        "actions/get-repo-public-key",
        "GET",
        "/api/v3/repos/alice/project/actions/secrets/public-key",
        Some(&writer),
        Value::Null,
        200,
    )
    .await;
    assert_ne!(key, actions_key);
    let input = sealed(&key, b"dependabot-only-value");
    for (name, status) in [
        ("registry_token", 201),
        ("REGISTRY_TOKEN", 204),
        ("SECOND", 201),
    ] {
        request(
            &router,
            "dependabot/create-or-update-repo-secret",
            "PUT",
            &format!("{base}/{name}"),
            Some(&writer),
            input.clone(),
            status,
        )
        .await;
    }
    let (headers, list) = request(
        &router,
        "dependabot/list-repo-secrets",
        "GET",
        &format!("{base}?per_page=1"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list["total_count"], 2);
    assert_eq!(list["secrets"][0]["name"], "REGISTRY_TOKEN");
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    assert!(list["secrets"][0].get("encrypted_value").is_none());
    assert!(list["secrets"][0].get("value").is_none());
    let (_, page) = request(
        &router,
        "dependabot/list-repo-secrets",
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
        "dependabot/get-repo-secret",
        "GET",
        &format!("{base}/registry_token"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(meta, list["secrets"][0]);
    let gh_list = super::action_secrets::gh_list(
        &router,
        "dependabot/list-repo-secrets",
        base,
        &reader,
        "secrets",
    )
    .await;
    assert_eq!(gh_list["secrets"][0], meta);
    assert_eq!(gh_list["secrets"][1], page["secrets"][0]);
    // The same name is independently usable by Actions, with a different key and value.
    request(
        &router,
        "actions/create-or-update-repo-secret",
        "PUT",
        "/api/v3/repos/alice/project/actions/secrets/REGISTRY_TOKEN",
        Some(&writer),
        sealed(&actions_key, b"actions-only-value"),
        201,
    )
    .await;
    assert_eq!(
        codeyard_db_actions::secrets::decrypt_repository_secrets(f.pool(), &encryptor, repo.id)
            .await
            .unwrap(),
        vec![("REGISTRY_TOKEN".into(), "actions-only-value".into())]
    );
    let (encrypted, nonce): (Vec<u8>, Vec<u8>) = match f.pool() {
        codeyard_database::DbPool::Sqlite(pool) => sqlx::query_as("SELECT encrypted_value,nonce FROM repo_dependabot_secrets WHERE repo_id=$1 AND name='REGISTRY_TOKEN'")
            .bind(repo.id).fetch_one(pool).await.unwrap(),
        codeyard_database::DbPool::Postgres(pool) => sqlx::query_as("SELECT encrypted_value,nonce FROM repo_dependabot_secrets WHERE repo_id=$1 AND name='REGISTRY_TOKEN'")
            .bind(repo.id).fetch_one(pool).await.unwrap(),
    };
    assert_ne!(encrypted, b"dependabot-only-value");
    assert_eq!(
        encryptor.decrypt(&encrypted, &nonce).unwrap(),
        b"dependabot-only-value"
    );
    for (token, expected) in [(None, 404), (Some(stranger.as_str()), 404)] {
        request(
            &router,
            "dependabot/list-repo-secrets",
            "GET",
            base,
            token,
            Value::Null,
            expected,
        )
        .await;
    }
    request(
        &router,
        "dependabot/get-repo-secret",
        "GET",
        &format!("{base}/MISSING"),
        Some(&reader),
        Value::Null,
        404,
    )
    .await;
    for method in ["PUT", "DELETE"] {
        let op = if method == "PUT" {
            "dependabot/create-or-update-repo-secret"
        } else {
            "dependabot/delete-repo-secret"
        };
        request(
            &router,
            op,
            method,
            &format!("{base}/REGISTRY_TOKEN"),
            Some(&reader),
            input.clone(),
            403,
        )
        .await;
    }
    for (name, input) in [
        ("BAD-NAME", input.clone()),
        ("GITHUB_TOKEN", input.clone()),
        ("WRONG_KEY", sealed(&actions_key, b"wrong namespace")),
        (
            "WRONG_BOX",
            json!({"key_id":key["key_id"], "encrypted_value":sealed(&actions_key, b"wrong key")["encrypted_value"]}),
        ),
        (
            "BAD_BOX",
            json!({"key_id":key["key_id"], "encrypted_value":"not base64"}),
        ),
        ("BAD_INPUT", json!({})),
        ("NUL", sealed(&key, b"no\0nul")),
        ("LARGE", sealed(&key, &vec![b'x'; 49153])),
    ] {
        request(
            &router,
            "dependabot/create-or-update-repo-secret",
            "PUT",
            &format!("{base}/{name}"),
            Some(&writer),
            input,
            422,
        )
        .await;
    }
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        outsider,
        repo.id,
        codeyard_database::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    for (path, op) in [
        (base.to_owned(), "dependabot/list-repo-secrets"),
        (
            format!("{base}/public-key"),
            "dependabot/get-repo-public-key",
        ),
        (
            format!("{base}/REGISTRY_TOKEN"),
            "dependabot/get-repo-secret",
        ),
    ] {
        request(&router, op, "GET", &path, Some(&stranger), Value::Null, 403).await;
    }
    request(
        &router,
        "dependabot/create-or-update-repo-secret",
        "PUT",
        &format!("{base}/NO_ADMIN"),
        Some(&stranger),
        input,
        403,
    )
    .await;
    request(
        &router,
        "dependabot/delete-repo-secret",
        "DELETE",
        &format!("{base}/SECOND"),
        Some(&stranger),
        Value::Null,
        403,
    )
    .await;
    for status in [204, 404] {
        request(
            &router,
            "dependabot/delete-repo-secret",
            "DELETE",
            &format!("{base}/SECOND"),
            Some(&writer),
            Value::Null,
            status,
        )
        .await;
    }
    request(
        &router,
        "dependabot/delete-repo-secret",
        "DELETE",
        &format!("{base}/REGISTRY_TOKEN"),
        Some(&writer),
        Value::Null,
        204,
    )
    .await;
    assert_eq!(
        codeyard_db_actions::secrets::decrypt_repository_secrets(f.pool(), &encryptor, repo.id)
            .await
            .unwrap()[0]
            .1,
        "actions-only-value"
    );
    let client = crate::support::http_client::Client::login(&router, f.pool(), owner).await;
    let response = client
        .form_reply(
            "/alice/project/_forms/secret-save",
            &[
                ("repository_id", &repo.id.to_string()),
                ("secret_kind", "dependabot"),
                ("name", "FORM_ONLY"),
                ("value", "never-echo-form-value"),
            ],
        )
        .await;
    assert_eq!(
        response.status,
        axum::http::StatusCode::SEE_OTHER,
        "{}",
        response.text()
    );
    assert!(response
        .location
        .as_deref()
        .unwrap()
        .ends_with("settings?tab=dependabot-secrets"));
    let page = client
        .page_ok("/alice/project/settings?tab=dependabot-secrets")
        .await;
    assert!(page.contains("Dependabot secrets"));
    assert!(page.contains("FORM_ONLY"));
    assert!(page.contains("name=\"secret_kind\" value=\"dependabot\""));
    assert!(!page.contains("never-echo-form-value"));
    let page = client.page_ok("/alice/project/settings?tab=secrets").await;
    assert!(!page.contains("FORM_ONLY"));
    assert!(codeyard_db_actions::secrets::decrypt_repository_secrets(
        f.pool(),
        &encryptor,
        repo.id
    )
    .await
    .unwrap()
    .iter()
    .all(|(name, _)| name != "FORM_ONLY"));
    let response = client
        .form_reply(
            "/alice/project/_forms/secret-delete",
            &[
                ("repository_id", &repo.id.to_string()),
                ("secret_kind", "dependabot"),
                ("name", "FORM_ONLY"),
                ("confirm", "yes"),
            ],
        )
        .await;
    assert_eq!(
        response.status,
        axum::http::StatusCode::SEE_OTHER,
        "{}",
        response.text()
    );
    let page = client
        .page_ok("/alice/project/settings?tab=dependabot-secrets")
        .await;
    assert!(!page.contains("FORM_ONLY"));
    f.finish().await;
}

async fn organization_secrets(pg: bool) {
    use codeyard_database::queries::organizations::OrganizationOutcome;
    use codeyard_db_orgs::organizations::profiles::NewOrganization;
    use codeyard_db_orgs::organizations::profiles::OrganizationProfile;
    use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
    use codeyard_db_repos::repositories::creation::RepositoryTarget;
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("alice").await;
    let member = f.actor("bob").await;
    let outsider = f.actor("eve").await;
    let OrganizationOutcome::Applied(org) =
        codeyard_db_orgs::organizations::profiles::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!("organization creation denied")
    };
    assert!(matches!(
        codeyard_db_orgs::organizations::membership::add_organization_member(
            f.pool(),
            owner,
            org.id,
            member,
            codeyard_database::models::OrgRole::Member
        )
        .await
        .unwrap(),
        OrganizationOutcome::Applied(_)
    ));
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for name in ["first", "second"] {
        repos.push(
            provision(
                (f.pool(), None),
                root.path(),
                RepositoryCreationOwner {
                    actor_id: owner,
                    target: RepositoryTarget::Organization(org.id),
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
    let foreign = provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "personal",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let encryptor = Arc::new(SecretEncryptor::new(&[0x65; 32]));
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
    let member_token = pat(&f, &router, member, "write").await;
    let stranger = pat(&f, &router, outsider, "write").await;
    let base = "/api/v3/orgs/acme/dependabot/secrets";
    let (_, key) = request(
        &router,
        "dependabot/get-org-public-key",
        "GET",
        &format!("{base}/public-key"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    let (_, again) = request(
        &router,
        "dependabot/get-org-public-key",
        "GET",
        &format!("{base}/public-key"),
        Some(&writer),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(key, again);
    let mut input = sealed(&key, b"organization-only-value");
    input["visibility"] = json!("selected");
    input["selected_repository_ids"] = json!([repos[0].id.to_string()]);
    request(
        &router,
        "dependabot/create-or-update-org-secret",
        "PUT",
        &format!("{base}/registry_token"),
        Some(&writer),
        input.clone(),
        201,
    )
    .await;
    request(
        &router,
        "dependabot/create-or-update-org-secret",
        "PUT",
        &format!("{base}/SECOND"),
        Some(&writer),
        input.clone(),
        201,
    )
    .await;
    let (headers, list) = request(
        &router,
        "dependabot/list-org-secrets",
        "GET",
        &format!("{base}?per_page=1"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list["total_count"], 2);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    assert_eq!(list["secrets"][0]["name"], "REGISTRY_TOKEN");
    assert!(list["secrets"][0]["selected_repositories_url"]
        .as_str()
        .unwrap()
        .ends_with("/orgs/acme/dependabot/secrets/REGISTRY_TOKEN/repositories"));
    assert!(!list.to_string().contains("organization-only-value"));
    let (_, secret) = request(
        &router,
        "dependabot/get-org-secret",
        "GET",
        &format!("{base}/registry_token"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(secret, list["secrets"][0]);
    let gh_list = super::action_secrets::gh_list(
        &router,
        "dependabot/list-org-secrets",
        base,
        &reader,
        "secrets",
    )
    .await;
    assert_eq!(gh_list["secrets"][0], secret);
    let selected = format!("{base}/REGISTRY_TOKEN/repositories");
    let (_, list) = request(
        &router,
        "dependabot/list-selected-repos-for-org-secret",
        "GET",
        &selected,
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list["total_count"], 1);
    assert_eq!(list["repositories"][0]["id"], repos[0].id);
    for _ in 0..2 {
        request(
            &router,
            "dependabot/add-selected-repo-to-org-secret",
            "PUT",
            &format!("{selected}/{}", repos[1].id),
            Some(&writer),
            Value::Null,
            204,
        )
        .await;
    }
    let (headers, list) = request(
        &router,
        "dependabot/list-selected-repos-for-org-secret",
        "GET",
        &format!("{selected}?per_page=1"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list["total_count"], 2);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    request(
        &router,
        "dependabot/remove-selected-repo-from-org-secret",
        "DELETE",
        &format!("{selected}/{}", repos[0].id),
        Some(&writer),
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "dependabot/set-selected-repos-for-org-secret",
        "PUT",
        &selected,
        Some(&writer),
        json!({"selected_repository_ids":[repos[0].id, repos[0].id]}),
        204,
    )
    .await;
    // An invalid member of a replacement set must not partially change that set.
    request(
        &router,
        "dependabot/set-selected-repos-for-org-secret",
        "PUT",
        &selected,
        Some(&writer),
        json!({"selected_repository_ids":[repos[1].id, foreign.id]}),
        422,
    )
    .await;
    let (_, list) = request(
        &router,
        "dependabot/list-selected-repos-for-org-secret",
        "GET",
        &selected,
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list["total_count"], 1);
    assert_eq!(list["repositories"][0]["id"], repos[0].id);
    request(
        &router,
        "dependabot/create-or-update-org-secret",
        "PUT",
        &format!("{base}/REGISTRY_TOKEN"),
        Some(&writer),
        json!({"visibility":"private"}),
        204,
    )
    .await;
    let (_, list) = request(
        &router,
        "dependabot/list-selected-repos-for-org-secret",
        "GET",
        &selected,
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list["total_count"], 0);
    request(
        &router,
        "dependabot/add-selected-repo-to-org-secret",
        "PUT",
        &format!("{selected}/{}", repos[0].id),
        Some(&writer),
        Value::Null,
        409,
    )
    .await;
    request(
        &router,
        "dependabot/remove-selected-repo-from-org-secret",
        "DELETE",
        &format!("{selected}/{}", repos[0].id),
        Some(&writer),
        Value::Null,
        409,
    )
    .await;
    for (input, name) in [
        (json!({"visibility":"bad"}), "REGISTRY_TOKEN"),
        (
            json!({"visibility":"all","selected_repository_ids":[]}),
            "REGISTRY_TOKEN",
        ),
        (json!({"visibility":"all"}), "NEW_WITHOUT_VALUE"),
        (
            json!({"visibility":"selected","selected_repository_ids":["not an id"]}),
            "REGISTRY_TOKEN",
        ),
        (
            json!({"visibility":"all","key_id":key["key_id"],"encrypted_value":"invalid"}),
            "REGISTRY_TOKEN",
        ),
    ] {
        request(
            &router,
            "dependabot/create-or-update-org-secret",
            "PUT",
            &format!("{base}/{name}"),
            Some(&writer),
            input,
            422,
        )
        .await;
    }
    request(
        &router,
        "dependabot/get-org-secret",
        "GET",
        &format!("{base}/MISSING"),
        Some(&reader),
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "dependabot/create-or-update-org-secret",
        "PUT",
        &format!("{base}/SECOND"),
        Some(&reader),
        input.clone(),
        403,
    )
    .await;
    request(
        &router,
        "dependabot/delete-org-secret",
        "DELETE",
        &format!("{base}/SECOND"),
        Some(&reader),
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "dependabot/list-org-secrets",
        "GET",
        base,
        None,
        Value::Null,
        401,
    )
    .await;
    request(
        &router,
        "dependabot/set-selected-repos-for-org-secret",
        "PUT",
        &selected,
        Some(&reader),
        json!({"selected_repository_ids":[]}),
        403,
    )
    .await;
    request(
        &router,
        "dependabot/add-selected-repo-to-org-secret",
        "PUT",
        &format!("{selected}/{}", repos[0].id),
        Some(&reader),
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "dependabot/remove-selected-repo-from-org-secret",
        "DELETE",
        &format!("{selected}/{}", repos[0].id),
        Some(&reader),
        Value::Null,
        403,
    )
    .await;
    for token in [&member_token, &stranger] {
        request(
            &router,
            "dependabot/list-org-secrets",
            "GET",
            base,
            Some(token),
            Value::Null,
            403,
        )
        .await;
        request(
            &router,
            "dependabot/get-org-public-key",
            "GET",
            &format!("{base}/public-key"),
            Some(token),
            Value::Null,
            403,
        )
        .await;
        request(
            &router,
            "dependabot/create-or-update-org-secret",
            "PUT",
            &format!("{base}/SECOND"),
            Some(token),
            input.clone(),
            403,
        )
        .await;
        request(
            &router,
            "dependabot/delete-org-secret",
            "DELETE",
            &format!("{base}/SECOND"),
            Some(token),
            Value::Null,
            403,
        )
        .await;
        request(
            &router,
            "dependabot/list-selected-repos-for-org-secret",
            "GET",
            &selected,
            Some(token),
            Value::Null,
            403,
        )
        .await;
        request(
            &router,
            "dependabot/set-selected-repos-for-org-secret",
            "PUT",
            &selected,
            Some(token),
            json!({"selected_repository_ids":[]}),
            403,
        )
        .await;
    }
    let (encrypted, nonce): (Vec<u8>, Vec<u8>) = match f.pool() {
        codeyard_database::DbPool::Sqlite(pool) => sqlx::query_as("SELECT encrypted_value,nonce FROM org_dependabot_secrets WHERE org_id=$1 AND name='REGISTRY_TOKEN'").bind(org.id).fetch_one(pool).await.unwrap(),
        codeyard_database::DbPool::Postgres(pool) => sqlx::query_as("SELECT encrypted_value,nonce FROM org_dependabot_secrets WHERE org_id=$1 AND name='REGISTRY_TOKEN'").bind(org.id).fetch_one(pool).await.unwrap(),
    };
    assert_ne!(encrypted, b"organization-only-value");
    assert_eq!(
        encryptor.decrypt(&encrypted, &nonce).unwrap(),
        b"organization-only-value"
    );
    for status in [204, 404] {
        request(
            &router,
            "dependabot/delete-org-secret",
            "DELETE",
            &format!("{base}/SECOND"),
            Some(&writer),
            Value::Null,
            status,
        )
        .await;
    }
    f.finish().await;
}

#[tokio::test]
async fn dependabot_organization_secrets_sqlite() {
    organization_secrets(false).await;
}

#[tokio::test]
async fn dependabot_organization_secrets_postgres() {
    organization_secrets(true).await;
}

#[tokio::test]
async fn dependabot_repository_secrets_sqlite() {
    repository_secrets(false).await;
}

#[tokio::test]
async fn dependabot_repository_secrets_postgres() {
    repository_secrets(true).await;
}

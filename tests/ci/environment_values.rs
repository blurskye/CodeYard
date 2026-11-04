use super::action_secrets::{pat, request, sealed};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use crate::support::http_client::Client;
use axum::http::StatusCode;

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_app::ci::config::CiConfig;
use codeyard_app::repositories::lifecycle;
use codeyard_app::storage::ObjectStore;
use codeyard_db_actions::environment_values as values;
use codeyard_db_actions::environment_values::Save;
use codeyard_db_orgs::organization_values::Kind;
use codeyard_support::secrets::SecretEncryptor;
use serde_json::{json, Value};
use std::sync::Arc;

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("alice").await;
    let collaborator = f.actor("bob").await;
    let outsider = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
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
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        collaborator,
        repo.id,
        codeyard_db_core::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    values::create(f.pool(), owner, repo.id, "Production")
        .await
        .unwrap();
    values::create(f.pool(), owner, repo.id, "production")
        .await
        .unwrap();
    assert_eq!(
        values::environments(f.pool(), owner, repo.id, 0)
            .await
            .unwrap(),
        vec!["Production"]
    );
    let canonical = codeyard_db_actions::environments::get(f.pool(), repo.id, "PRODUCTION")
        .await
        .unwrap()
        .unwrap();
    codeyard_db_actions::environments::upsert(
        f.pool(),
        owner,
        repo.id,
        "production",
        &codeyard_db_actions::environments::Update {
            wait_timer: Some(17),
            prevent_self_review: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    values::create(f.pool(), owner, repo.id, "PRODUCTION")
        .await
        .unwrap();
    let configured = codeyard_db_actions::environments::get(f.pool(), repo.id, "Production")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(configured.id, canonical.id);
    assert_eq!(configured.wait_timer, 17);
    assert!(configured.prevent_self_review);
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
    let collaborator_token = pat(&f, &router, collaborator, "write").await;
    let outside = pat(&f, &router, outsider, "write").await;
    let base = "/api/v3/repos/alice/project/environments/pRoDuCtIoN";
    let (_, key) = request(
        &router,
        "actions/get-environment-public-key",
        "GET",
        &format!("{base}/secrets/public-key"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    let (_, stable) = request(
        &router,
        "actions/get-environment-public-key",
        "GET",
        &format!("{base}/secrets/public-key"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(key, stable);
    let marker = "environment-private-marker";
    for status in [201, 204] {
        request(
            &router,
            "actions/create-or-update-environment-secret",
            "PUT",
            &format!("{base}/secrets/TOKEN"),
            Some(&writer),
            sealed(&key, marker.as_bytes()),
            status,
        )
        .await;
    }
    let (_, metadata) = request(
        &router,
        "actions/get-environment-secret",
        "GET",
        &format!("{base}/secrets/token"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(metadata["name"], "TOKEN");
    assert!(!metadata.to_string().contains(marker));
    let (_, list) = request(
        &router,
        "actions/list-environment-secrets",
        "GET",
        &format!("{base}/secrets?per_page=1"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list["total_count"], 1);
    assert!(!list.to_string().contains(marker));
    let gh_list = super::action_secrets::gh_list(
        &router,
        "actions/list-environment-secrets",
        &format!("{base}/secrets"),
        &reader,
        "secrets",
    )
    .await;
    assert_eq!(gh_list["secrets"][0], metadata);
    let stored = values::workflow_values(f.pool(), repo.id, "production", Kind::Secret)
        .await
        .unwrap();
    assert_eq!(stored[0].value, "");
    assert!(!stored[0]
        .encrypted_value
        .as_ref()
        .unwrap()
        .windows(marker.len())
        .any(|bytes| bytes == marker.as_bytes()));
    assert_eq!(
        values::for_run(f.pool(), Some(&encryptor), repo.id, "PRODUCTION", false)
            .await
            .unwrap()
            .secrets["TOKEN"],
        marker
    );
    assert!(
        values::for_run(f.pool(), None, repo.id, "production", false)
            .await
            .is_err()
    );
    assert!(values::for_run(f.pool(), None, repo.id, "production", true)
        .await
        .unwrap()
        .secrets
        .is_empty());
    request(
        &router,
        "actions/create-environment-variable",
        "POST",
        &format!("{base}/variables"),
        Some(&writer),
        json!({"name":"REGION","value":"readable environment variable"}),
        201,
    )
    .await;
    request(
        &router,
        "actions/create-environment-variable",
        "POST",
        &format!("{base}/variables"),
        Some(&writer),
        json!({"name":"region","value":"collision"}),
        409,
    )
    .await;
    let (_, variable) = request(
        &router,
        "actions/get-environment-variable",
        "GET",
        &format!("{base}/variables/region"),
        Some(&collaborator_token),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(variable["value"], "readable environment variable");
    request(
        &router,
        "actions/update-environment-variable",
        "PATCH",
        &format!("{base}/variables/REGION"),
        Some(&writer),
        json!({"name":"RENAMED","value":"updated environment variable"}),
        204,
    )
    .await;
    let (_, renamed) = request(
        &router,
        "actions/get-environment-variable",
        "GET",
        &format!("{base}/variables/RENAMED"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(renamed["created_at"], variable["created_at"]);
    assert_eq!(renamed["value"], "updated environment variable");
    let (_, variables) = request(
        &router,
        "actions/list-environment-variables",
        "GET",
        &format!("{base}/variables"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(variables["total_count"], 1);
    let gh_list = super::action_secrets::gh_list(
        &router,
        "actions/list-environment-variables",
        &format!("{base}/variables"),
        &reader,
        "variables",
    )
    .await;
    assert_eq!(gh_list["variables"][0], renamed);
    super::action_variables::gh_empty_value(
        &router,
        &format!(
            "/api/v3/repositories/{}/environments/Production/variables",
            repo.id
        ),
        &format!("{base}/variables"),
        &reader,
        &writer,
        None,
    )
    .await;
    for (token, status) in [(&reader, 403), (&outside, 404), (&collaborator_token, 403)] {
        request(
            &router,
            "actions/create-environment-variable",
            "POST",
            &format!(
                "/api/v3/repositories/{}/environments/Production/variables",
                repo.id
            ),
            Some(token),
            json!({"name":"DENIED","value":"not stored"}),
            status,
        )
        .await;
    }
    for token in [Some(outside.as_str()), None] {
        request(
            &router,
            "actions/list-environment-secrets",
            "GET",
            &format!("{base}/secrets"),
            token,
            Value::Null,
            404,
        )
        .await;
        request(
            &router,
            "actions/list-environment-variables",
            "GET",
            &format!("{base}/variables"),
            token,
            Value::Null,
            404,
        )
        .await;
    }
    for token in [&reader, &collaborator_token] {
        request(
            &router,
            "actions/create-or-update-environment-secret",
            "PUT",
            &format!("{base}/secrets/DENIED"),
            Some(token),
            sealed(&key, b"denied"),
            403,
        )
        .await;
        request(
            &router,
            "actions/create-environment-variable",
            "POST",
            &format!("{base}/variables"),
            Some(token),
            json!({"name":"DENIED","value":"denied"}),
            403,
        )
        .await;
        request(
            &router,
            "actions/update-environment-variable",
            "PATCH",
            &format!("{base}/variables/RENAMED"),
            Some(token),
            json!({"value":"denied"}),
            403,
        )
        .await;
        request(
            &router,
            "actions/delete-environment-variable",
            "DELETE",
            &format!("{base}/variables/RENAMED"),
            Some(token),
            Value::Null,
            403,
        )
        .await;
        request(
            &router,
            "actions/delete-environment-secret",
            "DELETE",
            &format!("{base}/secrets/TOKEN"),
            Some(token),
            Value::Null,
            403,
        )
        .await;
    }
    request(
        &router,
        "actions/get-environment-secret",
        "GET",
        &format!("{base}/secrets/TOKEN"),
        Some(&collaborator_token),
        Value::Null,
        403,
    )
    .await;
    for input in [
        json!({"name":"GITHUB_TOKEN","value":"bad"}),
        json!({"name":"BAD-NAME","value":"bad"}),
        json!({"name":"BAD","value":"nul\0value"}),
        json!({"name":"BIG","value":"x".repeat(49153)}),
    ] {
        let (_, body) = request(
            &router,
            "actions/create-environment-variable",
            "POST",
            &format!("{base}/variables"),
            Some(&writer),
            input,
            422,
        )
        .await;
        assert!(!body["errors"].as_array().unwrap().is_empty());
    }
    request(
        &router,
        "actions/create-or-update-environment-secret",
        "PUT",
        &format!("{base}/secrets/INVALID"),
        Some(&writer),
        json!({"key_id":"wrong","encrypted_value":"invalid"}),
        422,
    )
    .await;
    request(
        &router,
        "actions/create-or-update-environment-secret",
        "PUT",
        &format!("{base}/secrets/INVALID"),
        Some(&writer),
        sealed(&key, &[b'x'; 49153]),
        422,
    )
    .await;
    request(
        &router,
        "actions/delete-environment-secret",
        "DELETE",
        &format!("{base}/secrets/TOKEN"),
        Some(&writer),
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "actions/delete-environment-variable",
        "DELETE",
        &format!("{base}/variables/RENAMED"),
        Some(&writer),
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "actions/get-environment-secret",
        "GET",
        &format!("{base}/secrets/TOKEN"),
        Some(&reader),
        Value::Null,
        404,
    )
    .await;
    let client = Client::login(&router, f.pool(), owner).await;
    let id = repo.id.to_string();
    let reply = client
        .form_reply(
            "/alice/project/_forms/environment-create",
            &[("repository_id", &id), ("environment", "Staging")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    for (action, name, value) in [
        (
            "environment-secret-save",
            "UI_SECRET",
            "native-environment-private-marker",
        ),
        (
            "environment-variable-save",
            "UI_VARIABLE",
            "visible native variable",
        ),
    ] {
        let reply = client
            .form_reply(
                &format!("/alice/project/_forms/{action}"),
                &[
                    ("repository_id", &id),
                    ("environment", "staging"),
                    ("name", name),
                    ("value", value),
                ],
            )
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    }
    let max_secret = "🔐".repeat(12 * 1024);
    let max_variable = "💜".repeat(12 * 1024);
    for (action, name, value) in [
        ("environment-secret-save", "MAX_SECRET", max_secret.as_str()),
        (
            "environment-variable-save",
            "MAX_VARIABLE",
            max_variable.as_str(),
        ),
    ] {
        let reply = client
            .form_reply(
                &format!("/alice/project/_forms/{action}"),
                &[
                    ("repository_id", &id),
                    ("environment", "Staging"),
                    ("name", name),
                    ("value", value),
                ],
            )
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    }
    let (_, max) = request(
        &router,
        "actions/get-environment-variable",
        "GET",
        "/api/v3/repos/alice/project/environments/staging/variables/MAX_VARIABLE",
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(max["value"], max_variable);
    let page = client
        .page_ok("/alice/project/settings?tab=secrets&environment=STAGING")
        .await;
    assert!(
        page.contains("UI_SECRET")
            && page.contains("UI_VARIABLE")
            && page.contains("visible native variable")
    );
    assert!(!page.contains("native-environment-private-marker"));
    assert!(!page.contains(&max_secret));
    let reply = client
        .form_reply(
            "/alice/project/_forms/environment-secret-save",
            &[
                ("repository_id", &id),
                ("environment", "staging"),
                ("name", "GITHUB_DENIED"),
                ("value", "refused-secret-marker"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!reply.text().contains("refused-secret-marker"));
    let fork = values::for_run(f.pool(), None, repo.id, "staging", true)
        .await
        .unwrap();
    assert!(fork.secrets.is_empty());
    assert_eq!(fork.variables["UI_VARIABLE"], "visible native variable");
    for kind in [Kind::Secret, Kind::Variable] {
        assert!(matches!(
            values::save(
                f.pool(),
                Some(&encryptor),
                collaborator,
                repo.id,
                "staging",
                &Save {
                    kind,
                    previous: None,
                    name: "DENIED",
                    value: Some("denied")
                }
            )
            .await,
            Err(values::ValueError::Denied)
        ));
    }
    request(
        &router,
        "repos/delete-an-environment",
        "DELETE",
        base,
        Some(&writer),
        Value::Null,
        204,
    )
    .await;
    assert!(
        codeyard_db_actions::environments::get(f.pool(), repo.id, "production")
            .await
            .unwrap()
            .is_none()
    );
    for kind in [Kind::Secret, Kind::Variable] {
        assert!(
            values::workflow_values(f.pool(), repo.id, "production", kind)
                .await
                .unwrap()
                .is_empty()
        );
    }
    request(
        &router,
        "actions/list-environment-variables",
        "GET",
        &format!("{base}/variables"),
        Some(&reader),
        Value::Null,
        404,
    )
    .await;
    // As on GitHub, someone who can read a public repository but is not a collaborator may
    // not read its environment variables, like its repository variables.
    let open = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner),
        "open",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    values::create(f.pool(), owner, open.id, "Production")
        .await
        .unwrap();
    let open_base = "/api/v3/repos/alice/open/environments/Production/variables";
    request(
        &router,
        "actions/list-environment-variables",
        "GET",
        open_base,
        Some(&outside),
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "actions/get-environment-variable",
        "GET",
        &format!("{open_base}/NAME"),
        Some(&outside),
        Value::Null,
        403,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn sqlite_environment_values_contract() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_environment_values_contract() {
    contract(true).await;
}

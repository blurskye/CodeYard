use super::{assert_github_schema, request, Client, Database};
use axum::{http::StatusCode, Router};

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_repos_core::repositories::lifecycle;
use codeyard_app::repositories::secret_scanning::jobs;
use codeyard_ci::config::CiConfig;
use codeyard_db_security::secret_scanning as records;
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use codeyard_storage::ObjectStore;
use codeyard_support::secrets::SecretEncryptor;
use serde_json::{json, Value};
use std::sync::Arc;

async fn call(
    router: &Router,
    method: &str,
    path: &str,
    token: &str,
    input: Value,
    operation: &str,
    status: u16,
) -> Value {
    let (actual, _, body) = request(router, method, path, Some(token), input).await;
    assert_eq!(actual.as_u16(), status, "{method} {path}: {body}");
    if status != 204 {
        assert_github_schema(operation, status, &body);
    }
    body
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut fixture = Database::new(pg).await;
    fixture.complete_setup().await;
    let owner = fixture.actor("secret-owner").await;
    let outsider = fixture.actor("secret-outsider").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (fixture.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner),
        "private",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let state = AppState::new(
        fixture.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(Arc::new(SecretEncryptor::new(&[42; 32]))),
        CiConfig::default(),
        None,
    );
    let router = create_router(state.clone());
    let session = Client::login(&router, fixture.pool(), owner).await;
    let other_session = Client::login(&router, fixture.pool(), outsider).await;
    let mut values = Vec::new();
    for (actor, session, name, scope) in [
        (owner, &session.session, "writer", "write"),
        (owner, &session.session, "reader", "read"),
        (outsider, &other_session.session, "outsider", "write"),
    ] {
        let token = tokens::create(
            fixture.pool(),
            actor,
            Some(session),
            TokenInput {
                name,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        assert!(token.value.starts_with("cy_pat_"));
        values.push(format!("Bearer {}", token.value));
    }
    let (writer, reader, other) = (&values[0], &values[1], &values[2]);
    let base = "/api/v3/repos/secret-owner/private";
    let alerts = format!("{base}/secret-scanning/alerts");
    call(
        &router,
        "GET",
        &alerts,
        writer,
        Value::Null,
        "secret-scanning/list-alerts-for-repo",
        404,
    )
    .await;
    let secret = format!("ghp_{}", "2H8kL9mN4pQ7rS3tV6wX5yZ0aB1cD8eF9gJ2");
    let file = call(&router,"PUT",&format!("{base}/contents/config.env"),writer,json!({"message":"add config","content":base64::Engine::encode(&base64::engine::general_purpose::STANDARD,format!("TOKEN='{secret}'\n"))}),"repos/create-or-update-file-contents",201).await;
    let first_sha = file["commit"]["sha"].as_str().unwrap();
    let (status, disabled) = session
        .send(
            "GET",
            "/secret-owner/private/security/secret-scanning",
            "",
            String::new(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(disabled.contains("Secret scanning is disabled"));
    // Enable through the real plain form; the backfill job is inserted in the same transaction.
    let (status, _) = session
        .form(
            "/secret-owner/private/_forms/code-security",
            &[
                ("repository_id", &repo.id.to_string()),
                ("secret_scanning", "on"),
                ("push_protection", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(jobs::run_next(&state).await.unwrap());
    assert!(!jobs::run_next(&state).await.unwrap());
    let body = call(
        &router,
        "GET",
        &alerts,
        reader,
        Value::Null,
        "secret-scanning/list-alerts-for-repo",
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["secret"], secret);
    assert_eq!(body[0]["secret_type"], "github_personal_access_token");
    let number = body[0]["number"].as_i64().unwrap();
    let one = format!("{alerts}/{number}");
    let hidden = call(
        &router,
        "GET",
        &format!("{one}?hide_secret=true"),
        writer,
        Value::Null,
        "secret-scanning/get-alert",
        200,
    )
    .await;
    assert_eq!(hidden["secret"], "");
    let locations = call(
        &router,
        "GET",
        &format!("{one}/locations"),
        writer,
        Value::Null,
        "secret-scanning/list-locations-for-alert",
        200,
    )
    .await;
    assert_eq!(locations[0]["details"]["path"], "config.env");
    assert_eq!(locations[0]["details"]["commit_sha"], first_sha);
    assert_eq!(locations[0]["details"]["start_line"], 1);
    assert_eq!(locations[0]["details"]["start_column"], 8);
    call(
        &router,
        "GET",
        &one,
        other,
        Value::Null,
        "secret-scanning/get-alert",
        404,
    )
    .await;
    call(
        &router,
        "PATCH",
        &one,
        reader,
        json!({"state":"resolved","resolution":"revoked"}),
        "secret-scanning/update-alert",
        403,
    )
    .await;
    let assigned = call(
        &router,
        "PATCH",
        &one,
        writer,
        json!({"assignee":"secret-owner","validity":"active"}),
        "secret-scanning/update-alert",
        200,
    )
    .await;
    assert_eq!(assigned["assigned_to"]["login"], "secret-owner");
    assert_eq!(assigned["validity"], "active");
    for (query, count) in [
        (
            "page=1&per_page=1&validity=active,unknown&assignee=*&is_bypassed=false",
            1,
        ),
        ("assignee=none", 0),
        ("exclude_secret_types=github_personal_access_token", 0),
        ("secret_type=other,github_personal_access_token", 1),
    ] {
        let filtered = call(
            &router,
            "GET",
            &format!("{alerts}?{query}"),
            writer,
            Value::Null,
            "secret-scanning/list-alerts-for-repo",
            200,
        )
        .await;
        assert_eq!(filtered.as_array().unwrap().len(), count, "{query}");
    }
    call(
        &router,
        "GET",
        &format!("{alerts}?secret_type=x&exclude_secret_types=y"),
        writer,
        Value::Null,
        "secret-scanning/list-alerts-for-repo",
        422,
    )
    .await;
    let cleared = call(
        &router,
        "PATCH",
        &one,
        writer,
        json!({"assignee":null,"validity":null}),
        "secret-scanning/update-alert",
        200,
    )
    .await;
    assert_eq!(cleared["assigned_to"], Value::Null);
    assert_eq!(cleared["validity"], "unknown");
    for input in [
        json!({"assignee":"secret-outsider"}),
        json!({"validity":"invented"}),
        json!({"resolution_comment":"no requested change"}),
        json!({"state":"resolved"}),
        json!({"state":"resolved","resolution":"bogus"}),
        json!({"state":"open","resolution":"revoked"}),
    ] {
        let error = call(
            &router,
            "PATCH",
            &one,
            writer,
            input,
            "secret-scanning/update-alert",
            422,
        )
        .await;
        assert!(error["errors"].is_array());
    }
    for reason in ["false_positive", "wont_fix", "revoked", "used_in_tests"] {
        let closed = call(
            &router,
            "PATCH",
            &one,
            writer,
            json!({"state":"resolved","resolution":reason}),
            "secret-scanning/update-alert",
            200,
        )
        .await;
        assert_eq!(closed["resolution"], reason);
        assert_eq!(closed["resolved_by"]["login"], "secret-owner");
        let reopened = call(
            &router,
            "PATCH",
            &one,
            writer,
            json!({"state":"open"}),
            "secret-scanning/update-alert",
            200,
        )
        .await;
        assert_eq!(reopened["resolution"], Value::Null);
        assert_eq!(reopened["resolved_at"], Value::Null);
    }
    call(
        &router,
        "GET",
        &format!("{alerts}/9999"),
        writer,
        Value::Null,
        "secret-scanning/get-alert",
        404,
    )
    .await;
    let html = session
        .page_ok(&format!(
            "/secret-owner/private/security/secret-scanning/{number}"
        ))
        .await;
    assert!(html.contains("config.env"));
    assert!(
        !html.contains(&secret),
        "Credential value must not enter the page model"
    );
    let (status, _) = session
        .form(
            &format!("/secret-owner/private/security/secret-scanning/{number}"),
            &[
                ("repository_id", &repo.id.to_string()),
                ("state", "resolved"),
                ("resolution", "used_in_tests"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let closed = call(
        &router,
        "GET",
        &format!("{alerts}?state=resolved&secret_type=github_personal_access_token"),
        writer,
        Value::Null,
        "secret-scanning/list-alerts-for-repo",
        200,
    )
    .await;
    assert_eq!(closed.as_array().unwrap().len(), 1);
    // Push protection refuses the same secret saved through the contents API.
    let content = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("{secret}\n"),
    );
    let (status, _, body) = request(
        &router,
        "PUT",
        &format!("{base}/contents/other.env"),
        Some(writer),
        json!({"message":"second location","content":content.clone()}),
    )
    .await;
    assert_eq!(status.as_u16(), 422, "{body}");
    // An incremental scan sees another occurrence, without duplicating or reopening the alert.
    // Push protection is off only for that save, so the occurrence can be stored at all.
    let (status, _) = session
        .form(
            "/secret-owner/private/_forms/code-security",
            &[
                ("repository_id", &repo.id.to_string()),
                ("secret_scanning", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    call(
        &router,
        "PUT",
        &format!("{base}/contents/other.env"),
        writer,
        json!({"message":"second location","content":content}),
        "repos/create-or-update-file-contents",
        201,
    )
    .await;
    let (status, _) = session
        .form(
            "/secret-owner/private/_forms/code-security",
            &[
                ("repository_id", &repo.id.to_string()),
                ("secret_scanning", "on"),
                ("push_protection", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(jobs::run_next(&state).await.unwrap());
    let (status, headers, locations) = request(
        &router,
        "GET",
        &format!("{one}/locations?per_page=1"),
        Some(writer),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_github_schema("secret-scanning/list-locations-for-alert", 200, &locations);
    assert!(headers["link"].to_str().unwrap().contains("next"));
    let scans = call(
        &router,
        "GET",
        &format!("{base}/secret-scanning/scan-history"),
        writer,
        Value::Null,
        "secret-scanning/get-scan-history",
        200,
    )
    .await;
    assert_eq!(scans["backfill_scans"][0]["status"], "completed");
    assert_eq!(scans["incremental_scans"][0]["status"], "completed");
    let hash = codeyard_support::token_hash::token_hash(&secret);
    let placeholder = records::protection(
        fixture.pool(),
        owner,
        repo.id,
        &hash,
        "github_personal_access_token",
    )
    .await
    .unwrap()
    .unwrap();
    let bypass_url = format!("{base}/secret-scanning/push-protection-bypasses");
    call(
        &router,
        "POST",
        &bypass_url,
        writer,
        json!({"placeholder_id":placeholder.id,"reason":"bad"}),
        "secret-scanning/create-push-protection-bypass",
        422,
    )
    .await;
    call(
        &router,
        "POST",
        &bypass_url,
        reader,
        json!({"placeholder_id":placeholder.id,"reason":"used_in_tests"}),
        "secret-scanning/create-push-protection-bypass",
        403,
    )
    .await;
    let granted = call(
        &router,
        "POST",
        &bypass_url,
        writer,
        json!({"placeholder_id":placeholder.id,"reason":"used_in_tests"}),
        "secret-scanning/create-push-protection-bypass",
        200,
    )
    .await;
    assert_eq!(granted["token_type"], "github_personal_access_token");
    assert!(records::protection(
        fixture.pool(),
        owner,
        repo.id,
        &hash,
        "github_personal_access_token"
    )
    .await
    .unwrap()
    .is_none());
    call(
        &router,
        "POST",
        &bypass_url,
        writer,
        json!({"placeholder_id":"unknown","reason":"will_fix_later"}),
        "secret-scanning/create-push-protection-bypass",
        404,
    )
    .await;
    assert!(other_session.page("/secret-owner/private/security").await.0 == StatusCode::NOT_FOUND);
    let repo_body = call(&router, "GET", base, writer, Value::Null, "repos/get", 200).await;
    assert_eq!(
        repo_body["security_and_analysis"]["secret_scanning"]["status"],
        "enabled"
    );
    assert_eq!(
        fixture
            .scalar(
                "SELECT COUNT(*) FROM secret_scanning_alerts WHERE repo_id=$1",
                &[repo.id]
            )
            .await,
        1
    );
    custom_patterns(
        &fixture,
        &router,
        &session,
        &state,
        (owner, writer, reader, other),
        (repo.id, base),
    )
    .await;
    fixture.finish().await;
}

async fn custom_patterns(
    fixture: &Database,
    router: &Router,
    session: &Client,
    state: &AppState,
    credentials: (i64, &str, &str, &str),
    repository: (i64, &str),
) {
    let (owner, writer, reader, outsider) = credentials;
    let (repo_id, base) = repository;
    let input = codeyard_db_orgs::organizations::profiles::NewOrganization::new(
        "secret-org",
        codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(Some("Security"), None)
            .unwrap(),
    )
    .unwrap();
    let codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(org) =
        codeyard_db_orgs::organizations::profiles::create_organization(
            fixture.pool(),
            owner,
            &input,
        )
        .await
        .unwrap()
    else {
        panic!("create organization")
    };
    let root = state.repos_path.clone();
    let repo = lifecycle::provision(
        (fixture.pool(), None),
        &root,
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner {
            actor_id: owner,
            target: codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(
                org.id,
            ),
        },
        "org-private",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let orgbase = "/api/v3/orgs/secret-org";
    let org_repo_base = "/api/v3/repos/secret-org/org-private";
    let (status, _) = session
        .form(
            "/secret-org/org-private/_forms/code-security",
            &[
                ("repository_id", &repo.id.to_string()),
                ("secret_scanning", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let secret = format!("ghp_{}", "3H8kL9mN4pQ7rS3tV6wX5yZ0aB1cD8eF9gJ2");
    call(router,"PUT",&format!("{org_repo_base}/contents/key"),writer,json!({"message":"fixture","content":base64::Engine::encode(&base64::engine::general_purpose::STANDARD,secret.as_bytes())}),"repos/create-or-update-file-contents",201).await;
    while jobs::run_next(state).await.unwrap() {}
    let alerts = call(
        router,
        "GET",
        &format!("{orgbase}/secret-scanning/alerts"),
        writer,
        Value::Null,
        "secret-scanning/list-alerts-for-org",
        200,
    )
    .await;
    assert_eq!(alerts[0]["repository"]["id"], repo.id);
    assert_eq!(alerts[0]["secret"], secret);
    call(
        router,
        "GET",
        &format!("{orgbase}/secret-scanning/alerts"),
        outsider,
        Value::Null,
        "secret-scanning/list-alerts-for-org",
        403,
    )
    .await;
    for (prefix, scope, repo_id, form_prefix, organization) in [
        (base, "repo", repo_id, "/secret-owner/private", false),
        (orgbase, "org", repo.id, "/secret-org/org-private", true),
    ] {
        let path = format!("{prefix}/secret-scanning/custom-patterns");
        let create = format!("secret-scanning/bulk-create-{scope}-custom-patterns");
        let list = format!("secret-scanning/list-{scope}-custom-patterns");
        let update = format!("secret-scanning/update-{scope}-custom-pattern");
        let delete = format!("secret-scanning/bulk-delete-{scope}-custom-patterns");
        call(
            router,
            "POST",
            &path,
            reader,
            json!({"patterns":[{"name":"Example","pattern":"example_[A-Z0-9]{16}"}]}),
            &create,
            403,
        )
        .await;
        call(
            router,
            "POST",
            &path,
            writer,
            json!({"patterns":[{"name":"Invalid","pattern":"("}]}),
            &create,
            422,
        )
        .await;
        let created=call(router,"POST",&path,writer,json!({"patterns":[{"name":"Example","pattern":"example_[A-Z0-9]{16}"},{"name":"Second","pattern":"second_[A-Z0-9]{16}"}]}),&create,201).await;
        let pattern = &created["created_patterns"][0];
        let id = pattern["id"].as_i64().unwrap();
        let slug = pattern["slug"].as_str().unwrap();
        assert_eq!(pattern["state"], "unpublished");
        let listed = call(
            router,
            "GET",
            &format!("{path}?state=unpublished&push_protection=disabled"),
            writer,
            Value::Null,
            &list,
            200,
        )
        .await;
        assert_eq!(listed.as_array().unwrap().len(), 2);
        let item = format!("{path}/{id}");
        call(
            router,
            "PATCH",
            &item,
            writer,
            json!({"custom_pattern_version":"stale","pattern":"fixed_[A-Z0-9]{16}"}),
            &update,
            412,
        )
        .await;
        let changed=call(router,"PATCH",&item,writer,json!({"custom_pattern_version":pattern["custom_pattern_version"],"pattern":"fixed_[A-Z0-9]{16}","must_not_match":["EXAMPLE"]}),&update,200).await;
        let (status, _) = session
            .form(
                &format!("{form_prefix}/_forms/secret-pattern-publish"),
                &[
                    ("repository_id", &repo_id.to_string()),
                    ("pattern_id", &id.to_string()),
                    ("organization", if organization { "true" } else { "false" }),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let repo_api = if organization { org_repo_base } else { base };
        let path_to_file = format!("{repo_api}/contents/{scope}-custom");
        let save = json!({"message":"custom fixture","content":base64::Engine::encode(&base64::engine::general_purpose::STANDARD,b"fixed_Z7VJQ6HPLXWMN2R4\n")});
        let security = format!("{form_prefix}/_forms/code-security");
        let repository_id = repo_id.to_string();
        if !organization {
            // Push protection is on in this repository, and a published pattern blocks the save.
            let (status, _, body) =
                request(router, "PUT", &path_to_file, Some(writer), save.clone()).await;
            assert_eq!(status.as_u16(), 422, "{body}");
            let (status, _) = session
                .form(
                    &security,
                    &[("repository_id", &repository_id), ("secret_scanning", "on")],
                )
                .await;
            assert_eq!(status, StatusCode::SEE_OTHER);
        }
        call(
            router,
            "PUT",
            &path_to_file,
            writer,
            save,
            "repos/create-or-update-file-contents",
            201,
        )
        .await;
        if !organization {
            let (status, _) = session
                .form(
                    &security,
                    &[
                        ("repository_id", &repository_id),
                        ("secret_scanning", "on"),
                        ("push_protection", "on"),
                    ],
                )
                .await;
            assert_eq!(status, StatusCode::SEE_OTHER);
        }
        while jobs::run_next(state).await.unwrap() {}
        let found = call(
            router,
            "GET",
            &format!("{repo_api}/secret-scanning/alerts?secret_type={slug}"),
            writer,
            Value::Null,
            "secret-scanning/list-alerts-for-repo",
            200,
        )
        .await;
        assert_eq!(found.as_array().unwrap().len(), 1);
        let history = call(
            router,
            "GET",
            &format!("{repo_api}/secret-scanning/scan-history"),
            writer,
            Value::Null,
            "secret-scanning/get-scan-history",
            200,
        )
        .await;
        assert!(history["custom_pattern_backfill_scans"]
            .as_array()
            .unwrap()
            .iter()
            .any(|scan| scan["pattern_name"] == "Example"
                && scan["pattern_scope"]
                    == if organization {
                        "organization"
                    } else {
                        "repository"
                    }));
        if organization {
            let configs_path = format!("{orgbase}/secret-scanning/pattern-configurations");
            let configs = call(
                router,
                "GET",
                &configs_path,
                writer,
                Value::Null,
                "secret-scanning/list-org-pattern-configs",
                200,
            )
            .await;
            let version = &configs["pattern_config_version"];
            let patched=call(router,"PATCH",&configs_path,writer,json!({"pattern_config_version":version,"provider_pattern_settings":[{"token_type":"GITHUB_PERSONAL_ACCESS_TOKEN","push_protection_setting":"disabled"}],"custom_pattern_settings":[{"token_type":format!("cp_{id}"),"custom_pattern_version":changed["custom_pattern_version"],"push_protection_setting":"disabled"}]}),"secret-scanning/update-org-pattern-configs",200).await;
            assert!(patched["pattern_config_version"].is_string());
            call(
                router,
                "PATCH",
                &configs_path,
                writer,
                json!({"pattern_config_version":version}),
                "secret-scanning/update-org-pattern-configs",
                409,
            )
            .await;
            call(
                router,
                "PATCH",
                &configs_path,
                reader,
                json!({}),
                "secret-scanning/update-org-pattern-configs",
                403,
            )
            .await;
            let (_, disabled) = records::effective_patterns(fixture.pool(), repo_id)
                .await
                .unwrap();
            assert!(disabled
                .iter()
                .any(|kind| kind == "github_personal_access_token"));
        }
        call(
            router,
            "DELETE",
            &path,
            writer,
            json!({"patterns":[{"pattern_id":id,"custom_pattern_version":"stale"}]}),
            &delete,
            412,
        )
        .await;
        call(router,"DELETE",&path,writer,json!({"patterns":[{"pattern_id":id,"custom_pattern_version":changed["custom_pattern_version"]}],"post_delete_action":"resolve_alerts"}),&delete,204).await;
        let closed = call(
            router,
            "GET",
            &format!("{repo_api}/secret-scanning/alerts?secret_type={slug}&state=resolved&resolution=pattern_deleted"),
            writer,
            Value::Null,
            "secret-scanning/list-alerts-for-repo",
            200,
        )
        .await;
        assert_eq!(closed.as_array().unwrap().len(), 1);
        // GitHub closes the alerts of a deleted custom pattern as pattern_deleted.
        assert_eq!(closed[0]["resolution"], "pattern_deleted");
        assert_eq!(closed[0]["resolution_comment"], "Custom pattern deleted");
        call(
            router,
            "DELETE",
            &path,
            writer,
            json!({"patterns":[{"pattern_id":created["created_patterns"][1]["id"]}]}),
            &delete,
            204,
        )
        .await;
        let remaining = call(router, "GET", &path, writer, Value::Null, &list, 200).await;
        assert!(remaining.as_array().unwrap().is_empty());
    }
}

#[tokio::test]
async fn secret_scanning_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn secret_scanning_postgres() {
    contract(true).await;
}

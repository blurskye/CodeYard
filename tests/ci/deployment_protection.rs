//! Environment rule APIs and App decisions fence real CI claims on both engines.
use super::action_secrets::{pat, request};
use crate::support::{database::Database, fake_webui_bundle, http_client::Client};
use codeyard::backend::{
    db::{
        self,
        deployment_protection::{self as protection, Refusal, Review},
        github_apps::App,
        models::CiRun,
    },
    identity::tokens::token_hash,
    webhooks,
};
use serde_json::{json, Value};
use std::time::Duration;

async fn app(
    f: &Database,
    actor: i64,
    session: &str,
    slug: &str,
    repos: &[i64],
) -> (App, i64, i64) {
    let app = db::github_apps::register(
        f.pool(),
        actor,
        session,
        &App {
            id: 0,
            owner_id: actor,
            bot_id: 0,
            is_public: true,
            client_id: format!("client-{slug}"),
            slug: slug.into(),
            name: slug.into(),
            description: String::new(),
            homepage: "https://example.test".into(),
            public_key: String::new(),
            permissions: r#"{"metadata":"read","deployments":"write"}"#.into(),
            events: "deployment_protection_rule".into(),
            access_level: 3,
            created_at: chrono::Utc::now(),
        },
        "fixture-hash",
    )
    .await
    .unwrap()
    .unwrap();
    let installation = db::github_apps::install(
        f.pool(),
        actor,
        session,
        app.id,
        Some(actor),
        None,
        Some(repos),
    )
    .await
    .unwrap()
    .unwrap();
    db::github_app_hooks::save(
        f.pool(),
        app.id,
        &db::webhooks::WebhookFields {
            username: None,
            password: None,
            payload_url: "http://127.0.0.1:1/blocked".into(),
            content_type: "application/json".into(),
            events: "deployment_protection_rule".into(),
            active: true,
            ssl_verify: true,
        },
        None,
        false,
    )
    .await
    .unwrap();
    let token = db::github_apps::issue(
        f.pool(),
        app.id,
        installation.id,
        None,
        None,
        &token_hash(&format!("token-{slug}")),
        &app.permissions,
        "[]",
        true,
    )
    .await
    .unwrap()
    .unwrap();
    (app, installation.id, token.id)
}

async fn enqueue(f: &Database, repo: i64, actor: i64, attempt: Option<&CiRun>) -> CiRun {
    let sha = "a".repeat(40);
    let blobs = json!({".github/workflows/deploy.yml":sha});
    let input = db::ci::NewCiRun {
        repo_id: repo,
        triggered_by_id: actor,
        pipeline: "cd",
        event: "workflow_dispatch",
        pull_request_id: None,
        branch: "main",
        head_sha: &sha,
        workflow_path: ".github/workflows/deploy.yml",
        workflow_blobs: &blobs,
        workflow_inputs: None,
        environment: Some("Production"),
        manual: true,
    };
    if let Some(source) = attempt {
        db::ci::enqueue_ci_run_attempt(
            f.pool(),
            input,
            &[actor],
            db::ci::NewCiAttempt {
                source: source.id,
                source_uuid: source.run_id,
                credential: None,
                allow_active: false,
                selection: db::ci::CiReplaySelection::All,
                definition: None,
                debug: false,
            },
            &db::runners::Scheduling::default(),
            None,
        )
        .await
        .unwrap()
    } else {
        db::ci::enqueue_ci_run(f.pool(), input, &[actor])
            .await
            .unwrap()
    }
}

async fn decide(f: &Database, token: i64, run: &CiRun, state: Option<&str>) -> anyhow::Result<()> {
    protection::review(
        f.pool(),
        token,
        run.repo_id,
        run.workflow_run_id.unwrap_or(run.id),
        &Review {
            environment_name: "production",
            state,
            comment: Some("Health checks completed"),
        },
    )
    .await
}

fn refused(result: anyhow::Result<()>, expected: fn(&Refusal) -> bool) {
    let error = result.unwrap_err();
    assert!(
        error.downcast_ref::<Refusal>().is_some_and(expected),
        "{error:?}"
    );
}

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let repo = db::repositories::records::create_repository(
        f.pool(),
        alice,
        "project",
        None,
        true,
        "/fixture/not-executed.git",
    )
    .await
    .unwrap();
    let other = db::repositories::records::create_repository(
        f.pool(),
        alice,
        "other",
        None,
        true,
        "/fixture/other.git",
    )
    .await
    .unwrap();
    let env = db::environments::upsert(f.pool(), alice, repo.id, "production", &Default::default())
        .await
        .unwrap();
    let foreign_env =
        db::environments::upsert(f.pool(), alice, other.id, "production", &Default::default())
            .await
            .unwrap();
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let client = Client::login(&router, f.pool(), alice).await;
    let writer = pat(&f, &router, alice, "write").await;
    let reader = pat(&f, &router, alice, "read").await;
    let outsider = pat(&f, &router, bob, "write").await;
    let (provider, installation, token) =
        app(&f, alice, &client.session, "health", &[repo.id]).await;
    let (second, _, second_token) =
        app(&f, alice, &client.session, "other-health", &[repo.id]).await;
    let (excluded, _, _) = app(&f, alice, &client.session, "excluded", &[other.id]).await;
    let base = "/api/v3/repos/alice/project/environments/Production/deployment_protection_rules";
    let apps = format!("{base}/apps");
    let (headers, available) = request(
        &router,
        "repos/list-custom-deployment-rule-integrations",
        "GET",
        &format!("{apps}?per_page=1"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(available["total_count"], 2);
    assert!(headers.contains_key("link"));
    request(
        &router,
        "repos/list-custom-deployment-rule-integrations",
        "GET",
        &apps,
        Some(&outsider),
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "repos/create-deployment-protection-rule",
        "POST",
        base,
        Some(&reader),
        json!({"integration_id":provider.id}),
        403,
    )
    .await;
    request(
        &router,
        "repos/create-deployment-protection-rule",
        "POST",
        base,
        Some(&writer),
        json!({"integration_id":excluded.id}),
        422,
    )
    .await;
    assert!(
        protection::create(f.pool(), bob, repo.id, env.id, provider.id)
            .await
            .is_err()
    );
    assert!(
        protection::create(f.pool(), alice, repo.id, foreign_env.id, provider.id)
            .await
            .is_err()
    );
    let before = enqueue(&f, repo.id, alice, None).await;
    assert!(protection::gates(f.pool(), before.id)
        .await
        .unwrap()
        .is_empty());
    db::ci::request_ci_cancellation(f.pool(), before.run_id, alice)
        .await
        .unwrap();
    let (_, rule) = request(
        &router,
        "repos/create-deployment-protection-rule",
        "POST",
        base,
        Some(&writer),
        json!({"integration_id":provider.id}),
        201,
    )
    .await;
    let rule_id = rule["id"].as_i64().unwrap();
    let (_, duplicate) = request(
        &router,
        "repos/create-deployment-protection-rule",
        "POST",
        base,
        Some(&writer),
        json!({"integration_id":provider.id}),
        201,
    )
    .await;
    assert_eq!(rule, duplicate);
    request(
        &router,
        "repos/get-custom-deployment-protection-rule",
        "GET",
        &format!("{base}/{rule_id}"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    request(&router, "repos/get-custom-deployment-protection-rule", "GET", &format!("/api/v3/repos/alice/other/environments/production/deployment_protection_rules/{rule_id}"), Some(&reader), Value::Null, 404).await;
    let (_, listed) = request(
        &router,
        "repos/get-all-deployment-protection-rules",
        "GET",
        base,
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(listed["total_count"], 1);
    let run = enqueue(&f, repo.id, alice, None).await;
    let gates = protection::gates(f.pool(), run.id).await.unwrap();
    assert_eq!(gates.len(), 1);
    assert_eq!(gates[0].app_id, provider.id);
    assert_eq!(gates[0].state, "pending");
    assert!(db::ci::claim_ci_run(f.pool(), "worker", "local", &[alice])
        .await
        .unwrap()
        .is_none());

    // The normal sender records its SSRF refusal; the payload still proves targeting and callback identity.
    webhooks::deployment_protection::requested(f.pool(), &run)
        .await
        .unwrap();
    let delivery_id = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let deliveries = db::github_app_hooks::deliveries(f.pool(), provider.id)
                .await
                .unwrap();
            if let Some(delivery) = deliveries.first() {
                break delivery.recorded.id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("gate webhook recorded");
    let delivery = db::github_app_hooks::delivery(f.pool(), provider.id, delivery_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivery.recorded.event, "deployment_protection_rule");
    assert_eq!(delivery.recorded.action.as_deref(), Some("requested"));
    assert!(delivery.recorded.error.is_some());
    let payload: Value = serde_json::from_str(&delivery.recorded.request_body).unwrap();
    assert_eq!(payload["installation"]["id"], installation);
    assert_eq!(payload["repository"]["id"], repo.id);
    // A CI deployment has no REST deployment row, so the commit is sent at the top level.
    assert_eq!(payload["deployment"], Value::Null);
    assert_eq!(payload["sha"], "a".repeat(40));
    assert_eq!(payload["environment"], "Production");
    assert!(payload["deployment_callback_url"]
        .as_str()
        .unwrap()
        .ends_with(&format!(
            "/actions/runs/{}/deployment_protection_rule",
            run.workflow_run_id.unwrap()
        )));
    assert!(!delivery.recorded.request_body.contains("disk_path"));
    assert!(db::github_app_hooks::deliveries(f.pool(), second.id)
        .await
        .unwrap()
        .is_empty());

    let owner_token = db::users::tokens::find_token(f.pool(), &token_hash(&writer))
        .await
        .unwrap()
        .unwrap();
    refused(
        decide(&f, owner_token.id, &run, Some("approved")).await,
        |error| matches!(error, Refusal::Denied),
    );
    refused(
        decide(&f, second_token, &run, Some("approved")).await,
        |error| matches!(error, Refusal::NotFound),
    );
    refused(decide(&f, token, &run, Some("success")).await, |error| {
        matches!(error, Refusal::Invalid)
    });
    db::github_apps::change_installation(f.pool(), provider.id, installation, "suspend")
        .await
        .unwrap();
    refused(decide(&f, token, &run, Some("approved")).await, |error| {
        matches!(error, Refusal::Denied)
    });
    assert!(protection::integrations(f.pool(), repo.id)
        .await
        .unwrap()
        .iter()
        .all(|app| app.id != provider.id));
    db::github_apps::change_installation(f.pool(), provider.id, installation, "unsuspend")
        .await
        .unwrap();
    // Suspension revokes old credentials, so issue another installation token.
    let token = db::github_apps::issue(
        f.pool(),
        provider.id,
        installation,
        None,
        None,
        &token_hash("replacement"),
        &provider.permissions,
        "[]",
        true,
    )
    .await
    .unwrap()
    .unwrap()
    .id;
    decide(&f, token, &run, None).await.unwrap();
    assert!(db::ci::claim_ci_run(f.pool(), "worker", "local", &[alice])
        .await
        .unwrap()
        .is_none());
    decide(&f, token, &run, Some("approved")).await.unwrap();
    decide(&f, token, &run, Some("approved")).await.unwrap();
    refused(decide(&f, token, &run, Some("rejected")).await, |error| {
        matches!(error, Refusal::Conflict)
    });
    assert_eq!(
        db::ci::claim_ci_run(f.pool(), "worker", "local", &[alice])
            .await
            .unwrap()
            .unwrap()
            .id,
        run.id
    );
    db::ci::finish_ci_run(f.pool(), run.id, "worker", "success", None)
        .await
        .unwrap()
        .unwrap();
    let retry = enqueue(&f, repo.id, alice, Some(&run)).await;
    assert_eq!(retry.workflow_run_id, run.workflow_run_id);
    assert_eq!(
        protection::gates(f.pool(), retry.id).await.unwrap()[0].state,
        "pending"
    );
    assert!(db::ci::claim_ci_run(f.pool(), "worker", "local", &[alice])
        .await
        .unwrap()
        .is_none());
    decide(&f, token, &retry, Some("rejected")).await.unwrap();
    assert!(db::ci::claim_ci_run(f.pool(), "worker", "local", &[alice])
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        db::ci::get_ci_run_by_id(f.pool(), repo.id, retry.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "failed"
    );
    assert_eq!(
        db::ci::get_ci_deployment_by_run_id(f.pool(), retry.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "failed"
    );

    let waiting = enqueue(&f, repo.id, alice, None).await;
    request(
        &router,
        "repos/disable-deployment-protection-rule",
        "DELETE",
        &format!("{base}/{rule_id}"),
        Some(&reader),
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "repos/disable-deployment-protection-rule",
        "DELETE",
        &format!("{base}/{rule_id}"),
        Some(&writer),
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "repos/get-custom-deployment-protection-rule",
        "GET",
        &format!("{base}/{rule_id}"),
        Some(&reader),
        Value::Null,
        404,
    )
    .await;
    assert_eq!(
        protection::gates(f.pool(), waiting.id).await.unwrap()[0].state,
        "disabled"
    );
    assert_eq!(
        db::ci::claim_ci_run(f.pool(), "worker", "local", &[alice])
            .await
            .unwrap()
            .unwrap()
            .id,
        waiting.id
    );
    db::ci::finish_ci_run(f.pool(), waiting.id, "worker", "success", None)
        .await
        .unwrap();
    protection::create(f.pool(), alice, repo.id, env.id, provider.id)
        .await
        .unwrap();
    let unavailable = enqueue(&f, repo.id, alice, None).await;
    db::github_apps::change_repository(f.pool(), alice, installation, repo.id, false)
        .await
        .unwrap();
    refused(
        decide(&f, token, &unavailable, Some("approved")).await,
        |error| matches!(error, Refusal::Denied),
    );
    assert!(db::ci::claim_ci_run(f.pool(), "worker", "local", &[alice])
        .await
        .unwrap()
        .is_none());
    db::environments::delete(f.pool(), alice, repo.id, "production")
        .await
        .unwrap();
    assert!(db::ci::claim_ci_run(f.pool(), "worker", "local", &[alice])
        .await
        .unwrap()
        .is_none());
    f.finish().await;
}

#[tokio::test]
async fn deployment_protection_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn deployment_protection_postgres() {
    contract(true).await;
}

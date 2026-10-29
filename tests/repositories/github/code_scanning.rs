mod databases;
mod pagination;
mod pulls;
mod webhooks;

use super::{checked, db, json, tokens, Client, Database, Router, StatusCode, TokenInput, Value};
use base64::{engine::general_purpose::STANDARD, Engine};
use codeyard::backend::code_scanning;
use std::io::Write;

fn sarif_document(category: &str, present: bool, line: i64) -> Value {
    let result = json!({"ruleId":"test/rule","ruleIndex":0,"level":"error","message":{"text":"Unsafe call"},"partialFingerprints":{"primaryLocationLineHash":"stable"},"locations":[{"physicalLocation":{"artifactLocation":{"uri":"README.md"},"region":{"startLine":line}}}]});
    json!({"version":"2.1.0","runs":[{"automationDetails":{"id":category},"tool":{"driver":{"name":"Semgrep","version":"1.0","rules":[{"id":"test/rule","shortDescription":{"text":"Unsafe code"},"help":{"markdown":"Avoid unsafe calls."},"properties":{"security-severity":"8.5","tags":["security"]}}]}},"results":if present {vec![result]} else {vec![]}}]})
}
fn sarif(category: &str, present: bool, line: i64) -> String {
    encode_sarif(sarif_document(category, present, line))
}
fn encode_sarif(value: Value) -> String {
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(value.to_string().as_bytes()).unwrap();
    STANDARD.encode(gzip.finish().unwrap())
}

pub(super) async fn call(
    router: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
    op: &str,
    status: u16,
) -> Value {
    checked(
        router,
        op,
        method,
        path,
        Some(token),
        body,
        StatusCode::from_u16(status).unwrap(),
    )
    .await
    .1
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let mut credentials = Vec::new();
    for (actor, scope) in [(alice, "write"), (alice, "read"), (bob, "write")] {
        let client = Client::login(&router, f.pool(), actor).await;
        let token = tokens::create(
            f.pool(),
            actor,
            Some(&client.session),
            TokenInput {
                name: "scanning",
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let (writer, reader, outsider) = (&credentials[0], &credentials[1], &credentials[2]);
    call(
        &router,
        "POST",
        "/api/v3/user/repos",
        writer,
        json!({"name":"scan","private":true,"auto_init":true}),
        "repos/create-for-authenticated-user",
        201,
    )
    .await;
    let repo = db::repositories::records::get_repository(f.pool(), "alice", "scan")
        .await
        .unwrap()
        .unwrap();
    let hook = db::webhooks::insert_webhook(
        f.pool(),
        db::webhooks::HookOwner::Repository(repo.id),
        alice,
        &db::users::accounts::AccountCredential::Session(&crate::support::live_session(f.pool(), alice).await),
        &db::webhooks::WebhookFields {
            username: None,
            password: None,
            payload_url: "https://example.test/scanning".into(),
            content_type: "application/json".into(),
            events: "code_scanning_alert".into(),
            active: true,
            ssl_verify: true,
        },
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let base = "/api/v3/repos/alice/scan/code-scanning";
    let branch = call(
        &router,
        "GET",
        "/api/v3/repos/alice/scan/branches/main",
        writer,
        Value::Null,
        "repos/get-branch",
        200,
    )
    .await;
    let sha = branch["commit"]["sha"].as_str().unwrap();
    let payload =
        json!({"commit_sha":sha,"ref":"refs/heads/main","sarif":sarif("semgrep/",true,1)});
    call(
        &router,
        "POST",
        &format!("{base}/sarifs"),
        reader,
        payload.clone(),
        "code-scanning/upload-sarif",
        403,
    )
    .await;
    call(
        &router,
        "POST",
        &format!("{base}/sarifs"),
        outsider,
        payload.clone(),
        "code-scanning/upload-sarif",
        404,
    )
    .await;
    call(
        &router,
        "POST",
        &format!("{base}/sarifs"),
        writer,
        json!({"commit_sha":"bad","ref":"refs/heads/main","sarif":"bad"}),
        "code-scanning/upload-sarif",
        422,
    )
    .await;
    let receipt = call(
        &router,
        "POST",
        &format!("{base}/sarifs"),
        writer,
        payload.clone(),
        "code-scanning/upload-sarif",
        202,
    )
    .await;
    let status_url = format!("{base}/sarifs/{}", receipt["id"].as_str().unwrap());
    let status = call(
        &router,
        "GET",
        &status_url,
        reader,
        Value::Null,
        "code-scanning/get-sarif",
        200,
    )
    .await;
    assert_eq!(status["processing_status"], "pending");
    assert!(code_scanning::process_next(f.pool()).await.unwrap());
    let status = call(
        &router,
        "GET",
        &status_url,
        reader,
        Value::Null,
        "code-scanning/get-sarif",
        200,
    )
    .await;
    assert_eq!(status["processing_status"], "complete");
    let analyses = call(
        &router,
        "GET",
        &format!(
            "{base}/analyses?sarif_id={}",
            receipt["id"].as_str().unwrap()
        ),
        reader,
        Value::Null,
        "code-scanning/list-recent-analyses",
        200,
    )
    .await;
    assert_eq!(analyses.as_array().unwrap().len(), 1);
    let first = analyses[0]["id"].as_i64().unwrap();
    let analysis = call(
        &router,
        "GET",
        &format!("{base}/analyses/{first}"),
        reader,
        Value::Null,
        "code-scanning/get-analysis",
        200,
    )
    .await;
    assert_eq!(analysis["tool"]["name"], "Semgrep");
    let list = call(
        &router,
        "GET",
        &format!("{base}/alerts?state=open&tool_name=Semgrep&severity=high"),
        reader,
        Value::Null,
        "code-scanning/list-alerts-for-repo",
        200,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let number = list[0]["number"].as_i64().unwrap();
    let item = format!("{base}/alerts/{number}");
    let alert = call(
        &router,
        "GET",
        &item,
        reader,
        Value::Null,
        "code-scanning/get-alert",
        200,
    )
    .await;
    assert_eq!(alert["rule"]["help"], "Avoid unsafe calls.");
    let alice_session = Client::login(&router, f.pool(), alice).await;
    let page = alice_session
        .exchange(
            "GET",
            "/alice/scan/security/code-scanning",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("test/rule"));
    let page = alice_session
        .exchange(
            "GET",
            &format!("/alice/scan/security/code-scanning/{number}"),
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("Avoid unsafe calls."));
    assert!(page.text().contains("README.md"));
    let denied = Client::login(&router, f.pool(), bob)
        .await
        .exchange(
            "GET",
            "/alice/scan/security/code-scanning",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(denied.status, StatusCode::NOT_FOUND);
    let denied = alice_session
        .form_reply(
            &format!("/alice/scan/security/code-scanning/{number}/dismiss"),
            &[
                ("repository_id", "0"),
                ("state", "dismissed"),
                ("reason", "false positive"),
            ],
        )
        .await;
    assert_eq!(denied.status, StatusCode::CONFLICT);
    let native = alice_session
        .form_reply(
            &format!("/alice/scan/security/code-scanning/{number}/dismiss"),
            &[
                ("repository_id", &repo.id.to_string()),
                ("state", "dismissed"),
                ("reason", "false positive"),
            ],
        )
        .await;
    assert_eq!(native.status, StatusCode::SEE_OTHER);
    call(
        &router,
        "PATCH",
        &item,
        writer,
        json!({"state":"open"}),
        "code-scanning/update-alert",
        200,
    )
    .await;
    let mut delivered = false;
    for _ in 0..100 {
        let deliveries = db::webhooks::list_deliveries(f.pool(), hook).await.unwrap();
        if let Some(delivery) = deliveries.iter().find(|d| {
            d.event == "code_scanning_alert"
                && serde_json::from_str::<Value>(&d.request_body).unwrap()["action"] == "created"
        }) {
            let body: Value = serde_json::from_str(&delivery.request_body).unwrap();
            assert_eq!(body["alert"]["number"], number);
            assert_eq!(body["commit_oid"], sha);
            delivered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(delivered, "code scanning webhook must be enqueued");
    call(
        &router,
        "GET",
        &format!("{item}/instances"),
        reader,
        Value::Null,
        "code-scanning/list-alert-instances",
        200,
    )
    .await;
    call(
        &router,
        "PATCH",
        &item,
        reader,
        json!({"state":"dismissed","dismissed_reason":"false positive"}),
        "code-scanning/update-alert",
        403,
    )
    .await;
    call(
        &router,
        "PATCH",
        &item,
        writer,
        json!({"state":"dismissed"}),
        "code-scanning/update-alert",
        422,
    )
    .await;
    let dismissed=call(&router,"PATCH",&item,writer,json!({"state":"dismissed","dismissed_reason":"used in tests","dismissed_comment":"Fixture"}),"code-scanning/update-alert",200).await;
    assert_eq!(dismissed["dismissed_by"]["id"], alice);
    // A different category cannot fix this category's finding, and dismissals persist.
    for (category, present, line) in [
        ("trivy/", false, 1),
        ("semgrep/", true, 2),
        ("semgrep/", false, 1),
    ] {
        call(
            &router,
            "POST",
            &format!("{base}/sarifs"),
            writer,
            json!({"commit_sha":sha,"ref":"refs/heads/main","sarif":sarif(category,present,line)}),
            "code-scanning/upload-sarif",
            202,
        )
        .await;
        assert!(code_scanning::process_next(f.pool()).await.unwrap());
        let alert = call(
            &router,
            "GET",
            &item,
            reader,
            Value::Null,
            "code-scanning/get-alert",
            200,
        )
        .await;
        assert_eq!(alert["state"], "dismissed");
    }
    let reopened = call(
        &router,
        "PATCH",
        &item,
        writer,
        json!({"state":"open"}),
        "code-scanning/update-alert",
        200,
    )
    .await;
    assert_eq!(reopened["state"], "fixed");
    call(
        &router,
        "POST",
        &format!("{base}/sarifs"),
        writer,
        payload,
        "code-scanning/upload-sarif",
        202,
    )
    .await;
    assert!(code_scanning::process_next(f.pool()).await.unwrap());
    let reopened = call(
        &router,
        "GET",
        &item,
        reader,
        Value::Null,
        "code-scanning/get-alert",
        200,
    )
    .await;
    assert_eq!(reopened["state"], "open");
    assert_eq!(reopened["number"], number);
    webhooks::fixed_revision(&router, f.pool(), hook, writer, sha).await;
    let (headers, _) = checked(
        &router,
        "code-scanning/list-recent-analyses",
        "GET",
        &format!("{base}/analyses?per_page=1"),
        Some(reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    call(
        &router,
        "DELETE",
        &format!("{base}/analyses/{first}"),
        reader,
        Value::Null,
        "code-scanning/delete-analysis",
        403,
    )
    .await;
    call(
        &router,
        "DELETE",
        &format!("{base}/analyses/{first}"),
        writer,
        Value::Null,
        "code-scanning/delete-analysis",
        400,
    )
    .await;
    let analyses = call(
        &router,
        "GET",
        &format!("{base}/analyses"),
        reader,
        Value::Null,
        "code-scanning/list-recent-analyses",
        200,
    )
    .await;
    let latest = analyses
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["category"] == "semgrep")
        .unwrap();
    assert_eq!(latest["deletable"], true);
    assert_ne!(latest["id"], first);
    let id = latest["id"].as_i64().unwrap();
    let deletion = call(
        &router,
        "DELETE",
        &format!("{base}/analyses/{id}"),
        writer,
        Value::Null,
        "code-scanning/delete-analysis",
        200,
    )
    .await;
    assert!(deletion["next_analysis_url"].is_string());
    call(
        &router,
        "GET",
        &format!("{base}/analyses/{id}"),
        writer,
        Value::Null,
        "code-scanning/get-analysis",
        404,
    )
    .await;
    let restored = call(
        &router,
        "GET",
        &item,
        reader,
        Value::Null,
        "code-scanning/get-alert",
        200,
    )
    .await;
    assert_eq!(
        restored["state"], "fixed",
        "deleting the latest analysis restores the preceding empty analysis"
    );
    let final_id = analyses
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["category"] == "trivy")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    call(
        &router,
        "DELETE",
        &format!("{base}/analyses/{final_id}"),
        writer,
        Value::Null,
        "code-scanning/delete-analysis",
        400,
    )
    .await;
    let deleted = call(
        &router,
        "DELETE",
        &format!("{base}/analyses/{final_id}?confirm_delete=true"),
        writer,
        Value::Null,
        "code-scanning/delete-analysis",
        200,
    )
    .await;
    assert!(deleted["next_analysis_url"].is_null());
    assert!(deleted["confirm_delete_url"].is_null());
    call(
        &router,
        "GET",
        &format!("{base}/default-setup"),
        reader,
        Value::Null,
        "code-scanning/get-default-setup",
        200,
    )
    .await;
    call(
        &router,
        "PATCH",
        &format!("{base}/default-setup"),
        writer,
        json!({"state":"not-configured"}),
        "code-scanning/update-default-setup",
        200,
    )
    .await;
    call(
        &router,
        "PATCH",
        &format!("{base}/default-setup"),
        writer,
        json!({"state":"configured"}),
        "code-scanning/update-default-setup",
        422,
    )
    .await;
    call(
        &router,
        "POST",
        &format!("{base}/sarifs"),
        writer,
        json!({"commit_sha":sha,"ref":"refs/heads/main","sarif":STANDARD.encode(b"not gzip")}),
        "code-scanning/upload-sarif",
        202,
    )
    .await;
    let upload = db::code_scanning::pending(f.pool()).await.unwrap().unwrap();
    assert!(code_scanning::process_next(f.pool()).await.unwrap());
    let status = call(
        &router,
        "GET",
        &format!("{base}/sarifs/{}", upload.id),
        reader,
        Value::Null,
        "code-scanning/get-sarif",
        200,
    )
    .await;
    assert_eq!(status["processing_status"], "failed");
    assert!(!status["errors"].as_array().unwrap().is_empty());
    pulls::contract(&router, f.pool(), repo.id, alice, writer, sha).await;
    workflow_tokens(&router, f.pool(), repo.id, alice, sha, base).await;
    pagination::contract(&router, f.pool(), alice, writer, sha).await;
    call(
        &router,
        "GET",
        &format!("{base}/alerts"),
        outsider,
        Value::Null,
        "code-scanning/list-alerts-for-repo",
        404,
    )
    .await;
    for (method, suffix, operation) in [
        ("GET", "autofix", "code-scanning/get-autofix"),
        ("POST", "autofix", "code-scanning/create-autofix"),
        ("POST", "autofix/commits", "code-scanning/commit-autofix"),
    ] {
        call(
            &router,
            method,
            &format!("{item}/{suffix}"),
            writer,
            json!({}),
            operation,
            404,
        )
        .await;
    }
    f.execute("WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<1000) INSERT INTO code_scanning_uploads(id,repo_id,actor_id,ref_name,commit_sha,sarif,checkout_uri,created_at,processing_status) SELECT 'quota-' || CAST(n AS TEXT),u.repo_id,u.actor_id,u.ref_name,u.commit_sha,'',u.checkout_uri,u.created_at,'complete' FROM numbers CROSS JOIN code_scanning_uploads u WHERE u.sequence=(SELECT MIN(sequence) FROM code_scanning_uploads WHERE repo_id=$1 AND actor_id=$2)",&[repo.id,alice]).await;
    let limited = call(
        &router,
        "POST",
        &format!("{base}/sarifs"),
        writer,
        json!({"commit_sha":sha,"ref":"refs/heads/main","sarif":sarif("quota/",true,1)}),
        "code-scanning/upload-sarif",
        403,
    )
    .await;
    assert!(limited["message"]
        .as_str()
        .unwrap()
        .contains("API rate limit exceeded"));
    let input = db::organizations::profiles::NewOrganization::new(
        "acme",
        db::organizations::profiles::OrganizationProfile::new(Some("Acme"), None).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        db::organizations::profiles::create_organization(f.pool(), alice, &input)
            .await
            .unwrap(),
        db::organizations::OrganizationOutcome::Applied(_)
    ));
    call(
        &router,
        "POST",
        "/api/v3/repos/alice/scan/transfer",
        writer,
        json!({"new_owner":"acme"}),
        "repos/transfer",
        202,
    )
    .await;
    let alerts = call(
        &router,
        "GET",
        "/api/v3/orgs/acme/code-scanning/alerts?tool_name=Semgrep",
        reader,
        Value::Null,
        "code-scanning/list-alerts-for-org",
        200,
    )
    .await;
    assert_eq!(alerts.as_array().unwrap().len(), 1);
    assert_eq!(alerts[0]["repository"]["full_name"], "acme/scan");
    call(
        &router,
        "GET",
        "/api/v3/orgs/acme/code-scanning/alerts",
        outsider,
        Value::Null,
        "code-scanning/list-alerts-for-org",
        403,
    )
    .await;
    super::code_security::contract(&router, f.pool(), repo.id, alice, writer, reader, outsider)
        .await;
}

async fn workflow_tokens(
    router: &Router,
    pool: &db::DbPool,
    repo: i64,
    actor: i64,
    sha: &str,
    base: &str,
) {
    use codeyard::backend::ci;
    use std::collections::BTreeMap;
    let run = db::ci::enqueue_ci_run(
        pool,
        db::ci::NewCiRun {
            repo_id: repo,
            triggered_by_id: actor,
            pipeline: "security",
            event: "push",
            pull_request_id: None,
            branch: "main",
            head_sha: sha,
            workflow_path: ".github/workflows/security.yml",
            workflow_blobs: &json!({}),
            workflow_inputs: None,
            environment: None,
            manual: false,
        },
        &[actor],
    )
    .await
    .unwrap();
    let mut run = db::ci::claim_ci_run(pool, "scanner-worker", "local", &[actor])
        .await
        .unwrap()
        .filter(|r| r.id == run.id)
        .unwrap();
    for engine in ["codeyard-actions", "act"] {
        run.engine = Some(engine.into());
        let definitions = BTreeMap::from([(
            ".github/workflows/security.yml".into(),
            b"permissions: {contents: read, actions: read, security-events: write}\njobs: {scanner: {}}".to_vec(),
        )]);
        let tokens = ci::tokens::issue(pool, &run, &definitions, false)
            .await
            .unwrap();
        let token = if engine == "act" {
            &tokens.act
        } else {
            &tokens.workflows[".github/workflows/security.yml"]["scanner"]
        };
        call(
            router,
            "GET",
            "/api/v3/meta",
            &format!("Bearer {token}"),
            Value::Null,
            "meta/get",
            200,
        )
        .await;
        let metadata = call(
            router,
            "GET",
            &format!("/api/v3/repos/alice/scan/actions/runs/{}", run.id),
            &format!("Bearer {token}"),
            Value::Null,
            "actions/get-workflow-run",
            200,
        )
        .await;
        assert_eq!(metadata["id"], run.id);
        assert_eq!(metadata["path"], ".github/workflows/security.yml");
        assert_eq!(metadata["status"], "in_progress");
        assert!(metadata["workflow_url"]
            .as_str()
            .unwrap()
            .starts_with("http://localhost:3000/api/v3/repos/alice/scan/actions/workflows/"));
        for (method, input, operation) in [
            ("GET", Value::Null, "code-scanning/get-default-setup"),
            (
                "PATCH",
                json!({"state":"not-configured"}),
                "code-scanning/update-default-setup",
            ),
        ] {
            call(
                router,
                method,
                &format!("{base}/default-setup"),
                &format!("Bearer {token}"),
                input,
                operation,
                403,
            )
            .await;
        }
        // The action alias has no OpenAPI operation; its response is the public SARIF contract.
        databases::workflow_upload(router, token, 201).await;
        let accepted=call(
            router,
            "PUT",
            &format!("{base}/analysis"),
            &format!("Bearer {token}"),
            json!({"commit_oid":sha.to_ascii_uppercase(),"ref":"refs/heads/main","sarif":sarif(engine,true,1),"workflow_run_id":run.id}),
            "code-scanning/upload-sarif",
            202,
        )
        .await;
        assert!(code_scanning::process_next(pool).await.unwrap());
        let analyses = call(
            router,
            "GET",
            &format!(
                "{base}/analyses?sarif_id={}",
                accepted["id"].as_str().unwrap()
            ),
            &format!("Bearer {token}"),
            Value::Null,
            "code-scanning/list-recent-analyses",
            200,
        )
        .await;
        assert_eq!(analyses[0]["commit_sha"], sha);
        let definitions = BTreeMap::from([(
            ".github/workflows/security.yml".into(),
            b"permissions: {contents: write}\njobs: {scanner: {}}".to_vec(),
        )]);
        let tokens = ci::tokens::issue(pool, &run, &definitions, false)
            .await
            .unwrap();
        let token = if engine == "act" {
            &tokens.act
        } else {
            &tokens.workflows[".github/workflows/security.yml"]["scanner"]
        };
        databases::workflow_upload(router, token, 403).await;
        call(
            router,
            "POST",
            &format!("{base}/sarifs"),
            &format!("Bearer {token}"),
            json!({"commit_sha":sha,"ref":"refs/heads/main","sarif":sarif(engine,true,1)}),
            "code-scanning/upload-sarif",
            403,
        )
        .await;
        call(
            router,
            "PUT",
            &format!("{base}/analysis"),
            &format!("Bearer {token}"),
            json!({"commit_oid":sha,"ref":"refs/heads/main","sarif":sarif(engine,true,1)}),
            "code-scanning/upload-sarif",
            403,
        )
        .await;
    }
}

#[tokio::test]
async fn sqlite_code_scanning_contract() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_code_scanning_contract() {
    contract(true).await;
}

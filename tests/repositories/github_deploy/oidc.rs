//! Actions OIDC exchange verifies signatures using only public discovery material.
use crate::support::{database::Database, http_client::Client};
use axum::{
    body::{to_bytes, Body},
    http::Request,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use codeyard::backend::identity::tokens::{self, TokenInput};
use codeyard::backend::{ci, db, repositories};
use rsa::{
    pkcs1v15::{Signature, VerifyingKey},
    signature::Verifier,
    BigUint, RsaPublicKey,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tower::ServiceExt;

async fn get(router: &Router, path: &str, token: Option<&str>, expected: u16) -> Value {
    let mut request = Request::builder().uri(path);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        status.as_u16(),
        expected,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

fn verify(value: &Value, jwks: &Value) -> Value {
    let jwt = value["value"].as_str().unwrap();
    let parts: Vec<_> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
    assert_eq!(header["alg"], "RS256");
    let key = &jwks["keys"][0];
    assert_eq!(header["kid"], key["kid"]);
    let n = BigUint::from_bytes_be(&URL_SAFE_NO_PAD.decode(key["n"].as_str().unwrap()).unwrap());
    let e = BigUint::from_bytes_be(&URL_SAFE_NO_PAD.decode(key["e"].as_str().unwrap()).unwrap());
    let key = RsaPublicKey::new(n, e).unwrap();
    VerifyingKey::<sha2::Sha256>::new(key)
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &Signature::try_from(URL_SAFE_NO_PAD.decode(parts[2]).unwrap().as_slice()).unwrap(),
        )
        .unwrap();
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap()
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let actor = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repo = repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let sha = "a".repeat(40);
    let path = ".github/workflows/ci.yml";
    let run = db::ci::enqueue_ci_run(
        f.pool(),
        db::ci::NewCiRun {
            repo_id: repo.id,
            triggered_by_id: actor,
            pipeline: "ci",
            event: "push",
            pull_request_id: None,
            branch: "main",
            head_sha: &sha,
            workflow_path: path,
            workflow_blobs: &json!({}),
            workflow_inputs: None,
            environment: None,
            manual: false,
        },
        &[actor],
    )
    .await
    .unwrap();
    let run = db::ci::claim_ci_run(f.pool(), "worker", "local", &[actor], &codeyard::backend::ci::CiConfig::default().platform_labels())
        .await
        .unwrap()
        .filter(|r| r.id == run.id)
        .unwrap();
    let definitions = BTreeMap::from([(path.into(), b"name: Identity\npermissions: {id-token: write}\njobs:\n  writer: {}\n  production: {environment: 'Production:EU'}\n  reader: {permissions: {contents: read}}".to_vec())]);
    let credentials = ci::tokens::issue(f.pool(), &run, &definitions, false)
        .await
        .unwrap();
    assert!(credentials.oidc_jobs.contains(&format!("{path}:writer")));
    assert!(!credentials.oidc_jobs.contains(&format!("{path}:reader")));
    let jobs = &credentials.workflows[path];
    let router = f.router(root.path()).await;
    let discovery = get(&router, "/.well-known/openid-configuration", None, 200).await;
    assert_eq!(discovery["issuer"], ci::oidc::issuer());
    let jwks_uri = discovery["jwks_uri"]
        .as_str()
        .unwrap()
        .strip_prefix(&ci::oidc::issuer())
        .unwrap();
    let jwks = get(&router, jwks_uri, None, 200).await;
    let endpoint = "/api/ci/oidc/token?&audience=cloud%3Adeploy";
    get(&router, endpoint, None, 403).await;
    get(&router, endpoint, Some(&jobs["reader"]), 403).await;
    let first = get(&router, endpoint, Some(&jobs["writer"]), 200).await;
    let claims = verify(&first, &jwks);
    assert_eq!(claims["iss"], discovery["issuer"]);
    assert_eq!(claims["aud"], "cloud:deploy");
    assert_eq!(claims["sub"], "repo:alice/project:ref:refs/heads/main");
    assert_eq!(claims["repository"], "alice/project");
    assert_eq!(claims["repository_id"], repo.id.to_string());
    // User and organization ids overlap, so the owner id names its kind.
    assert_eq!(claims["repository_owner_id"], format!("user:{actor}"));
    assert_eq!(claims["actor"], "alice");
    assert_eq!(claims["actor_id"], actor.to_string());
    assert_eq!(claims["ref"], "refs/heads/main");
    assert_eq!(claims["sha"], sha);
    assert_eq!(claims["workflow"], "Identity");
    assert_eq!(claims["workflow_sha"], sha);
    assert_eq!(claims["job_workflow_sha"], sha);
    assert_eq!(claims["job_workflow_ref"], claims["workflow_ref"]);
    assert_eq!(claims["ref_type"], "branch");
    assert_eq!(
        claims["run_id"],
        run.workflow_run_id.unwrap_or(run.id).to_string()
    );
    assert_eq!(
        claims["run_number"],
        run.workflow_run_number.unwrap_or(run.id).to_string()
    );
    assert_eq!(
        claims["run_attempt"],
        run.workflow_run_attempt.unwrap_or(run.attempt).to_string()
    );
    assert_eq!(
        claims["workflow_ref"],
        format!("alice/project/{path}@refs/heads/main")
    );
    assert_eq!(claims["event_name"], "push");
    assert_eq!(claims["repository_visibility"], "private");
    assert_eq!(claims["runner_environment"], "self-hosted");
    assert_eq!(
        claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
        300
    );
    assert_eq!(claims["nbf"], claims["iat"]);
    assert!(claims["environment"].is_null());
    let default = get(
        &router,
        "/api/ci/oidc/token?",
        Some(&jobs["production"]),
        200,
    )
    .await;
    let default = verify(&default, &jwks);
    assert_eq!(
        default["sub"],
        "repo:alice/project:environment:Production%3AEU"
    );
    assert_eq!(default["environment"], "Production:EU");
    assert_eq!(default["aud"], format!("{}/alice", ci::oidc::issuer()));
    assert_ne!(default["jti"], claims["jti"]);
    assert_eq!(get(&router, jwks_uri, None, 200).await, jwks);
    let session = Client::login(&router, f.pool(), actor).await;
    let pat = tokens::create(
        f.pool(),
        actor,
        Some(&session.session),
        TokenInput {
            name: "settings",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let pat = format!("Bearer {}", pat.value);
    let template = "/api/v3/repos/alice/project/actions/oidc/customization/sub";
    let (_, settings) = super::checked(
        &router,
        "actions/get-custom-oidc-sub-claim-for-repo",
        "GET",
        template,
        Some(&pat),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(settings["use_default"], true);
    super::checked(
        &router,
        "actions/set-custom-oidc-sub-claim-for-repo",
        "PUT",
        template,
        Some(&pat),
        json!({"use_default":false,"include_claim_keys":["repository_owner","environment","repo"]}),
        201,
    )
    .await;
    let customized = verify(
        &get(&router, endpoint, Some(&jobs["production"]), 200).await,
        &jwks,
    );
    assert_eq!(
        customized["sub"],
        "repository_owner:alice:environment:Production%3AEU:repo:alice/project"
    );
    get(&router, endpoint, Some(&jobs["writer"]), 403).await;
    for keys in [
        json!([]),
        json!(["repo", "repo"]),
        json!(["sub"]),
        json!(["unknown-claim"]),
    ] {
        super::checked(
            &router,
            "actions/set-custom-oidc-sub-claim-for-repo",
            "PUT",
            template,
            Some(&pat),
            json!({"use_default":false,"include_claim_keys":keys}),
            422,
        )
        .await;
    }
    let page = session.page_ok("/alice/project/settings?tab=actions").await;
    assert!(page.contains("/alice/project/_forms/oidc-settings"));
    let (status, _) = session
        .form(
            "/alice/project/_forms/oidc-settings",
            &[
                ("repository_id", &repo.id.to_string()),
                ("include_claim_keys", "repo, context, job_workflow_ref"),
            ],
        )
        .await;
    assert_eq!(status.as_u16(), 303);
    let customized = verify(
        &get(&router, endpoint, Some(&jobs["writer"]), 200).await,
        &jwks,
    );
    assert_eq!(customized["sub"], format!("repo:alice/project:ref:refs/heads/main:job_workflow_ref:alice/project/{path}@refs/heads/main"));
    super::checked(
        &router,
        "actions/set-custom-oidc-sub-claim-for-repo",
        "PUT",
        template,
        Some(&pat),
        json!({"use_default":true,"include_claim_keys":["ignored"]}),
        201,
    )
    .await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let http = reqwest::Client::new();
    let discovery: Value = http
        .get(format!("{base}/.well-known/openid-configuration"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let jwks_path = discovery["jwks_uri"]
        .as_str()
        .unwrap()
        .strip_prefix(&ci::oidc::issuer())
        .unwrap();
    let public_keys: Value = http
        .get(format!("{base}{jwks_path}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let issued: Value = http
        .get(format!("{base}{endpoint}"))
        .bearer_auth(&jobs["writer"])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(verify(&issued, &public_keys)["sub"], claims["sub"]);
    assert_eq!(
        http.get(format!("{base}{endpoint}"))
            .bearer_auth(&jobs["reader"])
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        403
    );
    server.abort();
    db::ci_tokens::finish_job(f.pool(), run.id, "worker", &format!("{path}:writer"))
        .await
        .unwrap();
    get(&router, endpoint, Some(&jobs["writer"]), 403).await;
    f.execute(
        "UPDATE ci_runs SET cancel_requested=true WHERE id=$1",
        &[run.id],
    )
    .await;
    get(&router, endpoint, Some(&jobs["production"]), 403).await;
}

async fn organization_contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), alice).await;
    let mut credentials = Vec::new();
    for (actor, scope) in [(alice, "write"), (alice, "read"), (bob, "write")] {
        let client = Client::login(&router, f.pool(), actor).await;
        let token = tokens::create(
            f.pool(),
            actor,
            Some(&client.session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let (writer, reader, outsider) = (&credentials[0], &credentials[1], &credentials[2]);
    let input = db::organizations::profiles::NewOrganization::new(
        "acme",
        db::organizations::profiles::OrganizationProfile::new(None, None).unwrap(),
    )
    .unwrap();
    let db::organizations::OrganizationOutcome::Applied(org) =
        db::organizations::profiles::create_organization(f.pool(), alice, &input)
            .await
            .unwrap()
    else {
        panic!("create organization");
    };
    let repo = repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner {
            actor_id: alice,
            target: db::repositories::creation::RepositoryTarget::Organization(org.id),
        },
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let template = "/api/v3/orgs/acme/actions/oidc/customization/sub";
    let repo_template = "/api/v3/repos/acme/project/actions/oidc/customization/sub";
    let properties = "/api/v3/orgs/acme/actions/oidc/customization/properties/repo";
    for (token, expected) in [
        (None, 401),
        (Some(reader.as_str()), 403),
        (Some(outsider.as_str()), 403),
    ] {
        super::checked(
            &router,
            "oidc/update-oidc-custom-sub-template-for-org",
            "PUT",
            template,
            token,
            json!({"include_claim_keys":["repository_owner"]}),
            expected,
        )
        .await;
    }
    super::checked(
        &router,
        "oidc/get-oidc-custom-sub-template-for-org",
        "GET",
        template,
        Some(outsider),
        Value::Null,
        403,
    )
    .await;
    super::checked(
        &router,
        "actions/get-custom-oidc-sub-claim-for-repo",
        "GET",
        repo_template,
        Some(outsider),
        Value::Null,
        404,
    )
    .await;
    super::checked(
        &router,
        "actions/set-custom-oidc-sub-claim-for-repo",
        "PUT",
        repo_template,
        Some(reader),
        json!({"use_default":false}),
        403,
    )
    .await;
    let keys = json!(["repository_owner", "repo_property_business_unit", "context"]);
    super::checked(
        &router,
        "oidc/update-oidc-custom-sub-template-for-org",
        "PUT",
        template,
        Some(writer),
        json!({"include_claim_keys":keys}),
        201,
    )
    .await;
    let (_, settings) = super::checked(
        &router,
        "oidc/get-oidc-custom-sub-template-for-org",
        "GET",
        template,
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(settings["include_claim_keys"], keys);
    super::checked(
        &router,
        "orgs/custom-properties-for-repos-create-or-update-organization-definition",
        "PUT",
        "/api/v3/orgs/acme/properties/schema/business_unit",
        Some(writer),
        json!({"value_type":"string", "default_value":"payments:eu"}),
        200,
    )
    .await;
    super::checked(
        &router,
        "oidc/create-oidc-custom-property-inclusion-for-org",
        "POST",
        properties,
        Some(writer),
        json!({"custom_property_name":"unknown"}),
        422,
    )
    .await;
    super::checked(
        &router,
        "oidc/create-oidc-custom-property-inclusion-for-org",
        "POST",
        properties,
        Some(writer),
        json!({"custom_property_name":"business_unit"}),
        201,
    )
    .await;
    super::checked(
        &router,
        "oidc/create-oidc-custom-property-inclusion-for-org",
        "POST",
        properties,
        Some(writer),
        json!({"custom_property_name":"business_unit"}),
        422,
    )
    .await;
    let (_, inclusions) = super::checked(
        &router,
        "oidc/list-oidc-custom-property-inclusions-for-org",
        "GET",
        properties,
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(
        inclusions,
        json!([{"custom_property_name":"business_unit","inclusion_source":"organization"}])
    );
    let path = ".github/workflows/ci.yml";
    let sha = "b".repeat(40);
    let run = db::ci::enqueue_ci_run(
        f.pool(),
        db::ci::NewCiRun {
            repo_id: repo.id,
            triggered_by_id: alice,
            pipeline: "ci",
            event: "push",
            pull_request_id: None,
            branch: "main",
            head_sha: &sha,
            workflow_path: path,
            workflow_blobs: &json!({}),
            workflow_inputs: None,
            environment: None,
            manual: false,
        },
        &[alice],
    )
    .await
    .unwrap();
    let run = db::ci::claim_ci_run(f.pool(), "worker", "local", &[alice], &codeyard::backend::ci::CiConfig::default().platform_labels())
        .await
        .unwrap()
        .filter(|r| r.id == run.id)
        .unwrap();
    let definitions = BTreeMap::from([(
        path.into(),
        b"permissions: {id-token: write}\njobs: {deploy: {}}".to_vec(),
    )]);
    let credentials = ci::tokens::issue(f.pool(), &run, &definitions, false)
        .await
        .unwrap();
    let job = &credentials.workflows[path]["deploy"];
    let jwks = get(&router, "/.well-known/jwks", None, 200).await;
    let endpoint = "/api/ci/oidc/token?";
    let claims = verify(&get(&router, endpoint, Some(job), 200).await, &jwks);
    assert_eq!(claims["sub"], "repo:acme/project:ref:refs/heads/main");
    assert_eq!(claims["repo_property_business_unit"], "payments:eu");
    super::checked(
        &router,
        "actions/set-custom-oidc-sub-claim-for-repo",
        "PUT",
        repo_template,
        Some(writer),
        json!({"use_default":false}),
        201,
    )
    .await;
    let claims = verify(&get(&router, endpoint, Some(job), 200).await, &jwks);
    assert_eq!(
        claims["sub"],
        "repository_owner:acme:repo_property_business_unit:payments%3Aeu:ref:refs/heads/main"
    );
    assert_eq!(claims["repository_owner_id"], format!("org:{}", org.id));
    let page = session
        .page_ok(&format!("/settings/organizations/{}?tab=actions", org.id))
        .await;
    assert!(page.contains("/actions/oidc-save"));
    let (status, _) = session
        .form(
            &format!("/settings/organizations/{}/actions/oidc-save", org.id),
            &[
                ("org_id", &org.id.to_string()),
                ("include_claim_keys", "repository_id, context"),
            ],
        )
        .await;
    assert_eq!(status.as_u16(), 303);
    let claims = verify(&get(&router, endpoint, Some(job), 200).await, &jwks);
    assert_eq!(
        claims["sub"],
        format!("repository_id:{}:ref:refs/heads/main", repo.id)
    );
    let (status, _) = session
        .form(
            &format!(
                "/settings/organizations/{}/actions/oidc-property-delete",
                org.id
            ),
            &[
                ("org_id", &org.id.to_string()),
                ("custom_property_name", "business_unit"),
            ],
        )
        .await;
    assert_eq!(status.as_u16(), 303);
    assert!(verify(&get(&router, endpoint, Some(job), 200).await, &jwks)
        ["repo_property_business_unit"]
        .is_null());
    let (status, _) = session
        .form(
            &format!(
                "/settings/organizations/{}/actions/oidc-property-add",
                org.id
            ),
            &[
                ("org_id", &org.id.to_string()),
                ("custom_property_name", "business_unit"),
            ],
        )
        .await;
    assert_eq!(status.as_u16(), 303);
    super::checked(
        &router,
        "oidc/delete-oidc-custom-property-inclusion-for-org",
        "DELETE",
        &format!("{properties}/business_unit"),
        Some(writer),
        Value::Null,
        204,
    )
    .await;
    super::checked(
        &router,
        "oidc/delete-oidc-custom-property-inclusion-for-org",
        "DELETE",
        &format!("{properties}/business_unit"),
        Some(writer),
        Value::Null,
        404,
    )
    .await;
}

#[tokio::test]
async fn sqlite_actions_oidc_organization() {
    organization_contract(false).await;
}
#[tokio::test]
async fn postgres_actions_oidc_organization() {
    organization_contract(true).await;
}

#[tokio::test]
async fn sqlite_actions_oidc() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_actions_oidc() {
    contract(true).await;
}

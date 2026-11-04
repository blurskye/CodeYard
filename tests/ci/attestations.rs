use super::action_secrets::{pat, request};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use codeyard::backend::db;
use codeyard_app::ci;
use codeyard_app::repositories::lifecycle;
use serde_json::{json, Value};
use tower::ServiceExt;

fn pae(payload: &[u8]) -> Vec<u8> {
    let kind = "application/vnd.in-toto+json";
    let mut result = format!("DSSEv1 {} {kind} {} ", kind.len(), payload.len()).into_bytes();
    result.extend_from_slice(payload);
    result
}
fn signed(key: &iroh::SecretKey, predicate: &str) -> Value {
    let payload=json!({"_type":"https://in-toto.io/Statement/v1","subject":[{"name":"fixture","digest":{"sha256":"a".repeat(64)}}],"predicateType":predicate,"predicate":{}}).to_string();
    let signature = key.sign(&pae(payload.as_bytes()));
    json!({"mediaType":"application/vnd.dev.sigstore.bundle.v0.3+json","verificationMaterial":{"publicKey":{"hint":key.public().to_string()}},"dsseEnvelope":{"payloadType":"application/vnd.in-toto+json","payload":STANDARD.encode(payload),"signatures":[{"keyid":"fixture","sig":STANDARD.encode(signature.to_bytes())}]}})
}
async fn download(
    router: &Router,
    path: &str,
    token: Option<&str>,
    expected: StatusCode,
) -> Option<Value> {
    let parsed = url::Url::parse(path).unwrap();
    let mut request = Request::builder().uri(parsed.path());
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), expected);
    let body = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    if expected == StatusCode::OK {
        Some(serde_json::from_slice(&body).unwrap())
    } else {
        None
    }
}
async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("alice").await;
    let reader_id = f.actor("bob").await;
    let outsider = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (name, private) in [("private", true), ("other", true), ("public", false)] {
        repos.push(
            lifecycle::provision(
                (f.pool(), None),
                root.path(),
                codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner),
                name,
                None,
                private,
                None,
            )
            .await
            .unwrap(),
        );
    }
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        reader_id,
        repos[0].id,
        codeyard_db_core::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let writer = pat(&f, &router, owner, "write").await;
    let read_token = pat(&f, &router, owner, "read").await;
    let contributor = pat(&f, &router, reader_id, "write").await;
    let outside = pat(&f, &router, outsider, "write").await;
    let key = iroh::SecretKey::from_bytes(&[7; 32]);
    let proof = signed(&key, "https://slsa.dev/provenance/v1");
    let base = "/api/v3/repos/alice/private/attestations";
    let digest = format!("sha256:{}", "a".repeat(64));
    let mut ids = Vec::new();
    let proofs: Vec<_> = (0..3)
        .map(|index| {
            let mut proof = proof.clone();
            proof["verificationMaterial"]["fixture"] = json!(index);
            proof
        })
        .collect();
    for proof in &proofs {
        let (_, created) = request(
            &router,
            "repos/create-attestation",
            "POST",
            base,
            Some(&writer),
            json!({"bundle":proof}),
            201,
        )
        .await;
        let (_, repeated) = request(
            &router,
            "repos/create-attestation",
            "POST",
            base,
            Some(&writer),
            json!({"bundle":proof}),
            201,
        )
        .await;
        assert_eq!(created, repeated);
        ids.push(created["id"].as_i64().unwrap());
    }
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    let mut next = format!("{base}/{digest}?per_page=1");
    let mut visited = Vec::new();
    for (expected, proof) in ids.iter().zip(&proofs).rev() {
        let (headers, page) = request(
            &router,
            "repos/list-attestations",
            "GET",
            &next,
            Some(&read_token),
            Value::Null,
            200,
        )
        .await;
        assert_eq!(page["attestations"].as_array().unwrap().len(), 1);
        let row = &page["attestations"][0];
        assert_eq!(row["repository_id"], repos[0].id);
        assert_eq!(&row["bundle"], proof);
        assert_eq!(row["initiator"], "user");
        let url = row["bundle_url"].as_str().unwrap();
        assert!(url.ends_with(&format!("/bundles/{expected}")));
        let downloaded = download(&router, url, Some(&read_token), StatusCode::OK)
            .await
            .unwrap();
        assert_eq!(&downloaded, proof);
        let envelope = &downloaded["dsseEnvelope"];
        let payload = STANDARD
            .decode(envelope["payload"].as_str().unwrap())
            .unwrap();
        let expected_signature = key.sign(&pae(&payload));
        assert_eq!(
            STANDARD
                .decode(envelope["signatures"][0]["sig"].as_str().unwrap())
                .unwrap(),
            expected_signature.to_bytes()
        );
        key.public()
            .verify(&pae(&payload), &expected_signature)
            .unwrap();
        download(&router, url, Some(&outside), StatusCode::NOT_FOUND).await;
        download(&router, url, None, StatusCode::NOT_FOUND).await;
        visited.push(*expected);
        if let Some(link) = headers.get("link") {
            if let Some(link) = link
                .to_str()
                .unwrap()
                .split(", ")
                .find(|part| part.contains("rel=\"next\""))
            {
                let absolute = link.trim_start_matches('<').split_once('>').unwrap().0;
                let uri = url::Url::parse(absolute).unwrap();
                next = format!("{}?{}", uri.path(), uri.query().unwrap());
                continue;
            }
        }
        assert_eq!(expected, ids.first().unwrap());
    }
    assert_eq!(visited, ids.iter().rev().copied().collect::<Vec<_>>());
    let mut previous = URL_SAFE_NO_PAD.encode(format!("attestations:{}", ids[0]));
    for expected in &ids[1..] {
        let (headers, page) = request(
            &router,
            "repos/list-attestations",
            "GET",
            &format!("{base}/{digest}?per_page=1&before={previous}"),
            Some(&read_token),
            Value::Null,
            200,
        )
        .await;
        assert!(page["attestations"][0]["bundle_url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/bundles/{expected}")));
        previous = URL_SAFE_NO_PAD.encode(format!("attestations:{expected}"));
        let links = headers.get("link").unwrap().to_str().unwrap();
        assert!(links.contains("rel=\"next\""));
        assert_eq!(
            links.contains("rel=\"prev\""),
            expected != ids.last().unwrap()
        );
    }
    request(
        &router,
        "repos/list-attestations",
        "GET",
        &format!("{base}/{digest}"),
        Some(&contributor),
        Value::Null,
        200,
    )
    .await;
    for (token, status) in [
        (Some(read_token.as_str()), 403),
        (Some(contributor.as_str()), 403),
        (Some(outside.as_str()), 404),
        (None, 404),
    ] {
        request(
            &router,
            "repos/create-attestation",
            "POST",
            base,
            token,
            json!({"bundle":proof}),
            status,
        )
        .await;
    }
    for token in [Some(outside.as_str()), None] {
        request(
            &router,
            "repos/list-attestations",
            "GET",
            &format!("{base}/{digest}"),
            token,
            Value::Null,
            404,
        )
        .await;
    }
    let (_, empty) = request(
        &router,
        "repos/list-attestations",
        "GET",
        &format!("{base}/{digest}?predicate_type=sbom"),
        Some(&read_token),
        Value::Null,
        200,
    )
    .await;
    assert!(empty["attestations"].as_array().unwrap().is_empty());
    let sbom = signed(&key, "https://spdx.dev/Document");
    request(
        &router,
        "repos/create-attestation",
        "POST",
        base,
        Some(&writer),
        json!({"bundle":sbom}),
        201,
    )
    .await;
    for predicate in ["sbom", "https://spdx.dev/Document"] {
        let encoded: String = url::form_urlencoded::byte_serialize(predicate.as_bytes()).collect();
        let (_, page) = request(
            &router,
            "repos/list-attestations",
            "GET",
            &format!("{base}/{digest}?predicate_type={encoded}"),
            Some(&read_token),
            Value::Null,
            200,
        )
        .await;
        assert_eq!(page["attestations"].as_array().unwrap().len(), 1);
        assert_eq!(page["attestations"][0]["bundle"], sbom);
    }
    for query in [
        "after=bad",
        "after=bad&before=bad",
        "per_page=bad",
        "per_page=-1",
    ] {
        request(
            &router,
            "repos/list-attestations",
            "GET",
            &format!("{base}/{digest}?{query}"),
            Some(&read_token),
            Value::Null,
            422,
        )
        .await;
    }
    request(
        &router,
        "repos/list-attestations",
        "GET",
        &format!("{base}/invalid-digest"),
        Some(&read_token),
        Value::Null,
        422,
    )
    .await;
    for invalid in [
        json!({}),
        json!({"bundle":{}}),
        json!({"bundle":{"mediaType":"wrong"}}),
    ] {
        request(
            &router,
            "repos/create-attestation",
            "POST",
            base,
            Some(&writer),
            invalid,
            422,
        )
        .await;
    }
    let mut invalid = proof.clone();
    invalid["dsseEnvelope"]["payload"] = json!("invalid-base64");
    request(
        &router,
        "repos/create-attestation",
        "POST",
        base,
        Some(&writer),
        json!({"bundle":invalid}),
        422,
    )
    .await;
    let mut large = proof.clone();
    large["verificationMaterial"]["extra"] = json!("x".repeat(1024 * 1024));
    request(
        &router,
        "repos/create-attestation",
        "POST",
        base,
        Some(&writer),
        json!({"bundle":large}),
        422,
    )
    .await;
    // Job credentials use the same repository binding, granular permission and revocation
    // checks as every other Actions API, including the bundle download alias.
    let sha = "a".repeat(40);
    let run = codeyard_db_actions::ci::enqueue_ci_run(
        f.pool(),
        codeyard_db_actions::ci::NewCiRun {
            repo_id: repos[0].id,
            triggered_by_id: owner,
            pipeline: "ci",
            event: "push",
            pull_request_id: None,
            branch: "main",
            head_sha: &sha,
            workflow_path: ".github/workflows/ci.yml",
            workflow_blobs: &json!({}),
            workflow_inputs: None,
            environment: None,
            manual: false,
        },
        &[owner],
    )
    .await
    .unwrap();
    let claim = codeyard_db_actions::ci::claim_ci_run(
        f.pool(),
        "worker",
        "local",
        &[owner],
        &codeyard_app::ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .filter(|claim| claim.id == run.id)
    .unwrap();
    let definitions = std::collections::BTreeMap::from([(".github/workflows/ci.yml".into(),
        b"permissions: {}\njobs:\n  writer: {permissions: {attestations: write}}\n  reader: {permissions: {attestations: read}}\n  denied: {permissions: {contents: write}}".to_vec())]);
    let credentials = ci::tokens::issue(f.pool(), &claim, &definitions, false)
        .await
        .unwrap();
    let jobs = &credentials.workflows[".github/workflows/ci.yml"];
    let mut workflow_proof = proof.clone();
    workflow_proof["verificationMaterial"]["fixture"] = json!("workflow");
    request(
        &router,
        "repos/create-attestation",
        "POST",
        base,
        Some(&jobs["writer"]),
        json!({"bundle":workflow_proof}),
        201,
    )
    .await;
    for job in ["reader", "denied"] {
        request(
            &router,
            "repos/create-attestation",
            "POST",
            base,
            Some(&jobs[job]),
            json!({"bundle":proof}),
            403,
        )
        .await;
    }
    let (_, workflow_list) = request(
        &router,
        "repos/list-attestations",
        "GET",
        &format!("{base}/{digest}"),
        Some(&jobs["reader"]),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(workflow_list["attestations"][0]["initiator"], "workflow");
    let workflow_url = workflow_list["attestations"][0]["bundle_url"]
        .as_str()
        .unwrap();
    assert_eq!(
        download(&router, workflow_url, Some(&jobs["reader"]), StatusCode::OK)
            .await
            .unwrap(),
        workflow_proof
    );
    download(
        &router,
        workflow_url,
        Some(&jobs["denied"]),
        StatusCode::FORBIDDEN,
    )
    .await;
    request(
        &router,
        "repos/create-attestation",
        "POST",
        "/api/v3/repos/alice/other/attestations",
        Some(&jobs["writer"]),
        json!({"bundle":proof}),
        404,
    )
    .await;
    f.execute(
        "UPDATE actions_github_tokens SET revoked_at=0 WHERE run_id=$1",
        &[run.id],
    )
    .await;
    request(
        &router,
        "repos/create-attestation",
        "POST",
        base,
        Some(&jobs["writer"]),
        json!({"bundle":proof}),
        401,
    )
    .await;

    let public = "/api/v3/repos/alice/public/attestations";
    request(
        &router,
        "repos/create-attestation",
        "POST",
        public,
        Some(&writer),
        json!({"bundle":proof}),
        201,
    )
    .await;
    request(
        &router,
        "repos/list-attestations",
        "GET",
        &format!("{public}/{digest}"),
        None,
        Value::Null,
        401,
    )
    .await;
    let (_, public_list) = request(
        &router,
        "repos/list-attestations",
        "GET",
        &format!("{public}/{digest}?predicate_type=provenance"),
        Some(&read_token),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(public_list["attestations"].as_array().unwrap().len(), 1);
    let url = public_list["attestations"][0]["bundle_url"]
        .as_str()
        .unwrap();
    assert_eq!(
        download(&router, url, None, StatusCode::OK).await.unwrap(),
        proof
    );
    let wrong = url.replace("/public/", "/other/");
    download(&router, &wrong, Some(&writer), StatusCode::NOT_FOUND).await;
    f.finish().await;
}

#[tokio::test]
async fn sqlite_attestations_contract() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_attestations_contract() {
    contract(true).await;
}

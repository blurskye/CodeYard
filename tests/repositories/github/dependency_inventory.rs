use super::{
    assert_github_schema, request, tokens, Client, Database, StatusCode, TokenInput, Value,
};
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::json;
use tower::ServiceExt;

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("graph-owner").await;
    let stranger = f.actor("graph-stranger").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), owner).await;
    let outsider_session = Client::login(&router, f.pool(), stranger).await;
    let token = tokens::create(
        f.pool(),
        owner,
        Some(&session.session),
        TokenInput {
            name: "graph writer",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let writer = format!("Bearer {}", token.value);
    let token = tokens::create(
        f.pool(),
        owner,
        Some(&session.session),
        TokenInput {
            name: "graph reader",
            scope: "read",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let reader = format!("Bearer {}", token.value);
    let token = tokens::create(
        f.pool(),
        stranger,
        Some(&outsider_session.session),
        TokenInput {
            name: "outsider",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let outsider = format!("Bearer {}", token.value);
    let (status, _, repo) = request(
        &router,
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"graph","private":true,"auto_init":true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{repo}");
    let base = "/api/v3/repos/graph-owner/graph";
    // A private repository's graph is on once Dependabot alerts are, as on GitHub.
    let (status, _, body) = request(
        &router,
        "PUT",
        &format!("{base}/vulnerability-alerts"),
        Some(&writer),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status,_,manifest) = request(&router,"PUT",&format!("{base}/contents/package.json"),Some(&writer),json!({"message":"Declare dependency","content":STANDARD.encode(br#"{"dependencies":{"example":"^1.0.0"}}"#)})).await;
    assert_eq!(status, StatusCode::CREATED, "{manifest}");
    let lock = |version: &str| json!({"lockfileVersion":3,"packages":{"":{"dependencies":{"example":"^1.0.0"}},"node_modules/example":{"version":version,"resolved":format!("https://registry.npmjs.org/example/-/example-{version}.tgz")}}});
    let (status, _, old) = request(
        &router,
        "PUT",
        &format!("{base}/contents/package-lock.json"),
        Some(&writer),
        json!({"message":"Pin dependency","content":STANDARD.encode(lock("1.0.0").to_string())}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{old}");
    let old_sha = old["commit"]["sha"].as_str().unwrap();
    let sbom_path = format!("{base}/dependency-graph/sbom");
    let (status, _, body) = request(&router, "GET", &sbom_path, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_github_schema("dependency-graph/export-sbom", 200, &body);
    assert_eq!(body["sbom"]["packages"][0]["name"], "example");
    assert_eq!(body["sbom"]["packages"][0]["versionInfo"], "1.0.0");
    for credential in [None, Some(outsider.as_str())] {
        let (status, _, _) = request(&router, "GET", &sbom_path, credential, Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let generation = format!("{sbom_path}/generate-report");
    let (status, _, _) = request(&router, "HEAD", &generation, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM dependency_sbom_reports WHERE repo_id=$1",
            &[repo["id"].as_i64().unwrap()]
        )
        .await,
        0
    );
    let (status, _, generated) =
        request(&router, "GET", &generation, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::CREATED, "{generated}");
    assert_github_schema("dependency-graph/generate-sbom-report", 201, &generated);
    let url = url::Url::parse(generated["sbom_url"].as_str().unwrap()).unwrap();
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(url.path())
                .header("authorization", &reader)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    let download = url::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
    assert!(to_bytes(response.into_body(), 1024)
        .await
        .unwrap()
        .is_empty());
    let (_, _, generated_document) = request(
        &router,
        "GET",
        &format!("{}?{}", download.path(), download.query().unwrap()),
        Some(&reader),
        Value::Null,
    )
    .await;
    let (status,_,new) = request(&router,"PUT",&format!("{base}/contents/package-lock.json"),Some(&writer),json!({"message":"Update dependency","sha":old["content"]["sha"],"content":STANDARD.encode(lock("1.0.1").to_string())})).await;
    assert_eq!(status, StatusCode::OK, "{new}");
    let new_sha = new["commit"]["sha"].as_str().unwrap();
    let (status, _, pinned) = request(
        &router,
        "GET",
        &format!("{}?{}", download.path(), download.query().unwrap()),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{pinned}");
    assert_eq!(
        pinned, generated_document,
        "A generated report stays pinned after the branch moves"
    );
    let snapshot = json!({"version":0,"sha":new_sha,"ref":"refs/heads/main","job":{"id":"build-1","correlator":"unit"},"detector":{"name":"fixture","version":"1","url":"https://example.test/scanner"},"scanned":"2026-10-23T00:00:00Z","manifests":{"Cargo.lock":{"name":"Cargo.lock","resolved":{"dep":{"package_url":"pkg:cargo/test%2Dcrate@2.0.0%2Bbuild","scope":"development","relationship":"direct","dependencies":[]}}}}});
    let path = format!("{base}/dependency-graph/snapshots");
    let (status, _, denied) =
        request(&router, "POST", &path, Some(&reader), snapshot.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    let (status, _, created) =
        request(&router, "POST", &path, Some(&writer), snapshot.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_github_schema("dependency-graph/create-repository-snapshot", 201, &created);
    let (status, _, again) = request(&router, "POST", &path, Some(&writer), snapshot.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{again}");
    assert!(again["id"].as_i64().unwrap() > created["id"].as_i64().unwrap());
    let mut upper = snapshot.clone();
    upper["sha"] = json!(new_sha.to_ascii_uppercase());
    let (status, _, replaced) = request(&router, "POST", &path, Some(&writer), upper).await;
    assert_eq!(status, StatusCode::CREATED, "{replaced}");
    assert!(replaced["id"].as_i64().unwrap() > again["id"].as_i64().unwrap());
    let mut invalid = snapshot.clone();
    invalid["manifests"]["Cargo.lock"]["resolved"]["dep"]["dependencies"] = json!(["missing"]);
    let (status, _, body) = request(&router, "POST", &path, Some(&writer), invalid).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    for (field, value) in [("ref", "refs/heads/../main"), ("purl", "pkg:/")] {
        let mut invalid = snapshot.clone();
        if field == "ref" {
            invalid["ref"] = json!(value);
        } else {
            invalid["manifests"]["Cargo.lock"]["resolved"]["dep"]["package_url"] = json!(value);
        }
        let (status, _, body) = request(&router, "POST", &path, Some(&writer), invalid).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    for (correlator, scope) in [("runtime", "runtime"), ("later-development", "development")] {
        let mut supplemental = snapshot.clone();
        supplemental["job"]["correlator"] = json!(correlator);
        supplemental["manifests"]["Cargo.lock"]["resolved"]["dep"]["scope"] = json!(scope);
        let (status, _, body) = request(&router, "POST", &path, Some(&writer), supplemental).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let compare = format!("{base}/dependency-graph/compare/{old_sha}...{new_sha}");
    let (status, _, changes) = request(&router, "GET", &compare, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{changes}");
    assert_github_schema("dependency-graph/diff-range", 200, &changes);
    assert_eq!(changes.as_array().unwrap().len(), 3, "{changes}");
    assert!(changes
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["change_type"] == "removed" && p["version"] == "1.0.0"));
    assert!(changes
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "test-crate"
            && p["ecosystem"] == "rust"
            && p["version"] == "2.0.0+build"
            && p["scope"] == "runtime"));
    let (status, _, filtered) = request(
        &router,
        "GET",
        &format!("{compare}?name=Cargo.lock"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{filtered}");
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    let (status, html) = session.page("/graph-owner/graph/dependency-graph").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("test-crate"));
    assert!(html.contains("Export SBOM"));
    let (status, document) = session
        .page("/graph-owner/graph/dependency-graph/sbom")
        .await;
    assert_eq!(status, StatusCode::OK, "{document}");
    let document: Value = serde_json::from_str(&document).unwrap();
    assert_eq!(document["spdxVersion"], "SPDX-2.3");
    assert!(document["packages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "test-crate"));
    let (status, _, _) = request(
        &router,
        "GET",
        &format!("{base}/dependency-graph/compare/missing...main"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = request(&router, "GET", url.path(), Some(&outsider), Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    f.execute(
        "UPDATE repositories SET is_archived=true WHERE id=$1",
        &[repo["id"].as_i64().unwrap()],
    )
    .await;
    let (status, _, _) = request(&router, "POST", &path, Some(&writer), snapshot).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_dependency_graph_http_contract() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_dependency_graph_http_contract() {
    contract(true).await;
}

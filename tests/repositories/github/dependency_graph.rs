use super::{
    assert_github_schema, db, json, request, tokens, Client, Database, Router, StatusCode,
    TokenInput, Value,
};
use axum::{body::Body, http::Request};
use tower::ServiceExt;

const BASE: &str = "/api/v3/repos/alice/proj/dependency-graph";

async fn call(
    router: &Router,
    method: &str,
    suffix: &str,
    token: &str,
    body: Value,
    operation: &str,
    status: u16,
) -> Value {
    let (actual, _, body) = request(
        router,
        method,
        &format!("{BASE}{suffix}"),
        Some(token),
        body,
    )
    .await;
    assert_eq!(actual.as_u16(), status, "{method} {suffix}: {body}");
    assert_github_schema(&format!("dependency-graph/{operation}"), status, &body);
    body
}

fn snapshot(sha: &str, version: &str, scanned: &str) -> Value {
    json!({
        "version":0, "sha":sha, "ref":"refs/heads/main", "scanned":scanned,
        "job":{"id":"42", "correlator":"build_linux"},
        "detector":{"name":"test detector", "version":"1.0", "url":"https://example.test/detector"},
        "manifests":{"Cargo.lock":{"name":"Cargo.lock", "file":{"source_location":"Cargo.lock"}, "resolved":{
            "core":{"package_url":format!("pkg:cargo/example@{version}"), "relationship":"direct", "scope":"runtime", "dependencies":["child"]},
            "child":{"package_url":"pkg:npm/%40scope/child@2.0", "relationship":"indirect", "scope":"development"}
        }}}
    })
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), alice).await;
    let other_session = Client::login(&router, f.pool(), bob).await;
    let mut credentials = Vec::new();
    for (actor, session, scope) in [
        (alice, &session, "write"),
        (alice, &session, "read"),
        (bob, &other_session, "write"),
    ] {
        let token = tokens::create(
            f.pool(),
            actor,
            Some(&session.session),
            TokenInput {
                name: "dependency graph",
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let (writer, reader, outsider) = (&credentials[0], &credentials[1], &credentials[2]);
    // As on GitHub, a private repository's graph is off until Dependabot alerts are enabled.
    let (status, _, body) = request(
        &router,
        "GET",
        &format!("{BASE}/sbom"),
        Some(writer),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _, body) = request(
        &router,
        "PUT",
        "/api/v3/repos/alice/proj/vulnerability-alerts",
        Some(writer),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _, created) = request(
        &router,
        "PUT",
        "/api/v3/repos/alice/proj/contents/README.md",
        Some(writer),
        json!({"message":"Initial commit", "content":"aGVsbG8K"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let first = created["commit"]["sha"].as_str().unwrap();
    let blob = created["content"]["sha"].as_str().unwrap();
    let (status, _, created) = request(
        &router,
        "PUT",
        "/api/v3/repos/alice/proj/contents/README.md",
        Some(writer),
        json!({"message":"Next commit", "content":"d29ybGQK", "sha":blob}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let second = created["commit"]["sha"].as_str().unwrap();
    let first_snapshot = snapshot(first, "1.0", "2026-01-01T00:00:00Z");
    let second_snapshot = snapshot(second, "2.0", "2026-01-02T00:00:00Z");
    for snapshot in [&first_snapshot, &second_snapshot] {
        let body = call(
            &router,
            "POST",
            "/snapshots",
            writer,
            snapshot.clone(),
            "create-repository-snapshot",
            201,
        )
        .await;
        assert_eq!(body["result"], "SUCCESS");
        assert!(body["id"].as_i64().unwrap() > 0);
    }
    // Arrival order cannot let an older scan replace the current dependency graph.
    let old = call(
        &router,
        "POST",
        "/snapshots",
        writer,
        first_snapshot.clone(),
        "create-repository-snapshot",
        201,
    )
    .await;
    assert_eq!(old["result"], "ACCEPTED");
    let exported = call(
        &router,
        "GET",
        "/sbom",
        reader,
        Value::Null,
        "export-sbom",
        200,
    )
    .await;
    assert_eq!(exported["sbom"]["packages"].as_array().unwrap().len(), 2);
    let packages = exported["sbom"]["packages"].as_array().unwrap();
    assert!(packages
        .iter()
        .any(|p| p["name"] == "example" && p["versionInfo"] == "2.0"));
    assert!(packages.iter().any(|p| p["name"] == "@scope/child"));
    assert!(exported["sbom"]["relationships"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["relationshipType"] == "DEPENDS_ON"));

    let range = format!("/compare/{first}...{second}");
    let changes = call(
        &router,
        "GET",
        &range,
        reader,
        Value::Null,
        "diff-range",
        200,
    )
    .await;
    assert_eq!(changes.as_array().unwrap().len(), 2);
    assert_eq!(changes[0]["change_type"], "removed");
    assert_eq!(changes[0]["version"], "1.0");
    assert_eq!(changes[1]["change_type"], "added");
    assert_eq!(changes[1]["version"], "2.0");
    let (status, headers, page) = request(
        &router,
        "GET",
        &format!("{BASE}{range}?name=Cargo.lock&per_page=1"),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page.as_array().unwrap().len(), 1);
    assert!(headers["link"]
        .to_str()
        .unwrap()
        .contains("name=Cargo.lock"));
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let empty = call(
        &router,
        "GET",
        &format!("{range}?name=package.json"),
        reader,
        Value::Null,
        "diff-range",
        200,
    )
    .await;
    assert_eq!(empty, json!([]));
    let empty = call(
        &router,
        "GET",
        &format!("{range}?page=3&per_page=1"),
        reader,
        Value::Null,
        "diff-range",
        200,
    )
    .await;
    assert_eq!(empty, json!([]));
    let report = call(
        &router,
        "GET",
        "/sbom/generate-report",
        reader,
        Value::Null,
        "generate-sbom-report",
        201,
    )
    .await;
    let poll = url::Url::parse(report["sbom_url"].as_str().unwrap()).unwrap();
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(poll.path())
                .header("authorization", reader)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    let download = url::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
    // A completed export is immutable, even after another scan has been submitted.
    let newer = snapshot(second, "3.0", "2026-01-03T00:00:00Z");
    call(
        &router,
        "POST",
        "/snapshots",
        writer,
        newer,
        "create-repository-snapshot",
        201,
    )
    .await;
    let (status, _, downloaded) = request(
        &router,
        "GET",
        &format!("{}?{}", download.path(), download.query().unwrap()),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{downloaded}");
    assert_github_schema("dependency-graph/export-sbom", 200, &downloaded);
    assert!(downloaded["sbom"]["packages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["versionInfo"] == "2.0"));
    let count = f
        .scalar(
            "SELECT COUNT(*) FROM dependency_sbom_reports WHERE repo_id=$1",
            &[repo.id],
        )
        .await;
    let (status, _, _) = request(
        &router,
        "HEAD",
        &format!("{BASE}/sbom/generate-report"),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM dependency_sbom_reports WHERE repo_id=$1",
            &[repo.id]
        )
        .await,
        count
    );
    call(
        &router,
        "GET",
        "/sbom/fetch-report/missing",
        reader,
        Value::Null,
        "fetch-sbom-report",
        404,
    )
    .await;
    call(
        &router,
        "GET",
        "/sbom?report=missing",
        reader,
        Value::Null,
        "export-sbom",
        404,
    )
    .await;

    for token in [None, Some(outsider.as_str())] {
        for suffix in [
            "/sbom",
            "/sbom/generate-report",
            "/sbom/fetch-report/missing",
            &range,
        ] {
            let (status, _, body) = request(
                &router,
                "GET",
                &format!("{BASE}{suffix}"),
                token,
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        }
    }
    for (token, expected) in [
        (None, 401),
        (Some(reader.as_str()), 403),
        (Some(outsider.as_str()), 404),
    ] {
        let (status, _, body) = request(
            &router,
            "POST",
            &format!("{BASE}/snapshots"),
            token,
            first_snapshot.clone(),
        )
        .await;
        assert_eq!(status.as_u16(), expected, "{body}");
    }
    for (field, invalid) in [
        ("sha", json!("short")),
        ("ref", json!("main")),
        ("scanned", json!("yesterday")),
        (
            "manifests",
            json!({"bad":{"name":"bad", "resolved":{"bad":{"scope":"invalid"}}}}),
        ),
    ] {
        let mut input = first_snapshot.clone();
        input[field] = invalid;
        call(
            &router,
            "POST",
            "/snapshots",
            writer,
            input,
            "create-repository-snapshot",
            422,
        )
        .await;
    }
    // The native scanner's identity is reserved.
    let mut native = first_snapshot.clone();
    native["detector"]["name"] = json!("Codeyard");
    native["job"]["correlator"] = json!("lockfiles");
    call(
        &router,
        "POST",
        "/snapshots",
        writer,
        native,
        "create-repository-snapshot",
        422,
    )
    .await;
    // A detector/correlator is independent of the other submitted sources and branches.
    let mut independent = snapshot(second, "4.0", "2026-01-04T00:00:00Z");
    independent["job"]["correlator"] = json!("build_windows");
    call(
        &router,
        "POST",
        "/snapshots",
        writer,
        independent.clone(),
        "create-repository-snapshot",
        201,
    )
    .await;
    independent["ref"] = json!("refs/heads/other");
    independent["job"]["correlator"] = json!("other_branch");
    independent["manifests"]["Cargo.lock"]["resolved"]["core"]["package_url"] =
        json!("pkg:cargo/hidden@1.0");
    call(
        &router,
        "POST",
        "/snapshots",
        writer,
        independent,
        "create-repository-snapshot",
        201,
    )
    .await;
    let exported = call(
        &router,
        "GET",
        "/sbom",
        reader,
        Value::Null,
        "export-sbom",
        200,
    )
    .await;
    assert_eq!(exported["sbom"]["packages"].as_array().unwrap().len(), 3);
    let html = session.page_ok("/alice/proj/insights/dependencies").await;
    assert!(html.contains("example"), "{html}");
    assert!(html.contains("Export current branch SPDX SBOM"), "{html}");
    assert!(!html.contains("pkg:cargo/hidden"), "{html}");
    let (status, _) = Client::anonymous(&router)
        .send(
            "GET",
            "/alice/proj/insights/dependencies",
            "text/html",
            String::new(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Archived repositories cannot receive a new snapshot, regardless of token scope.
    f.execute(
        "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
        &[repo.id],
    )
    .await;
    call(
        &router,
        "POST",
        "/snapshots",
        writer,
        first_snapshot,
        "create-repository-snapshot",
        403,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn sqlite_dependency_graph() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_dependency_graph() {
    contract(true).await;
}

async fn vulnerability_contract(pg: bool) {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use codeyard::backend::{
        api::create_router, app::AppState, ci::CiConfig, storage::ObjectStore, vulnerabilities,
    };
    use db::organizations::{
        profiles::{NewOrganization, OrganizationProfile},
        OrganizationOutcome,
    };
    use std::sync::Arc;
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("vuln-owner").await;
    let outsider = f.actor("vuln-outsider").await;
    let OrganizationOutcome::Applied(org) = db::organizations::profiles::create_organization(
        f.pool(),
        owner,
        &NewOrganization::new("vuln-org", OrganizationProfile::new(None, None).unwrap()).unwrap(),
    )
    .await
    .unwrap() else {
        panic!("organization creation refused")
    };
    let root = tempfile::tempdir().unwrap();
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    let router = create_router(state.clone());
    let session = Client::login(&router, f.pool(), owner).await;
    let outsider_session = Client::login(&router, f.pool(), outsider).await;
    let mut credentials = Vec::new();
    for (actor, client, scope) in [
        (owner, &session, "write"),
        (owner, &session, "read"),
        (outsider, &outsider_session, "write"),
    ] {
        let token = tokens::create(
            f.pool(),
            actor,
            Some(&client.session),
            TokenInput {
                name: "vulnerability alerts",
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let (writer, reader, denied) = (&credentials[0], &credentials[1], &credentials[2]);
    let (status, _, created) = request(
        &router,
        "POST",
        "/api/v3/orgs/vuln-org/repos",
        Some(writer),
        json!({"name":"scan","private":true,"auto_init":true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let repo_id = created["id"].as_i64().unwrap();
    let config = db::code_security::save(
        f.pool(),
        owner,
        org.id,
        None,
        &json!({"name":"Dependencies","dependabot_alerts":"enabled"}),
    )
    .await
    .unwrap();
    db::code_security::attach(
        f.pool(),
        owner,
        org.id,
        Some(config.id),
        "selected",
        &[repo_id],
    )
    .await
    .unwrap();
    let base = "/api/v3/repos/vuln-org/scan";
    let alerts = format!("{base}/dependabot/alerts");
    let fixture = include_bytes!("../../../src/backend/vulnerabilities/fixtures/OSV-2026-1.json");
    // The mirror download path, served by a local fixture instead of osv.dev.
    let archive = {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("OSV-2026-1.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(fixture).unwrap();
        zip.finish().unwrap().into_inner()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mirror = format!("http://{}", listener.local_addr().unwrap());
    let mirror_routes = axum::Router::new().route(
        "/crates.io/all.zip",
        axum::routing::get(move || {
            let archive = archive.clone();
            async move { archive }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, mirror_routes).await });
    vulnerabilities::synchronize(
        f.pool(),
        &db::vulnerabilities::Settings {
            base_url: mirror,
            ecosystems: r#"["crates.io"]"#.into(),
        },
    )
    .await
    .unwrap();
    server.abort();
    assert!(!vulnerabilities::ingest(f.pool(), fixture).await.unwrap());
    let lock = |version: &str| {
        format!("version = 3\n[[package]]\nname = \"example\"\nversion = \"{version}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n")
    };
    let (status, _, created) = request(
        &router,
        "PUT",
        &format!("{base}/contents/Cargo.lock"),
        Some(writer),
        json!({"message":"Lock a vulnerable dependency","content":STANDARD.encode(lock("1.0.0"))}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let initial_commit = created["commit"]["sha"].as_str().unwrap().to_owned();
    let blob = created["content"]["sha"].as_str().unwrap().to_owned();
    // Run the same durable job the scheduler claims, with no external network.
    // Every new repository queues its first scan, so the PostgreSQL fixture's repository may
    // come first; it is claimed and left alone.
    let generation = loop {
        let (claimed, generation) = db::vulnerabilities::claim(f.pool())
            .await
            .unwrap()
            .expect("a Git publication must queue a scan");
        if claimed == repo_id {
            break generation;
        }
    };
    vulnerabilities::scan(&state, &f.repository(repo_id).await, generation)
        .await
        .unwrap();
    let viewer = db::dependency_graph::Reader::Viewer(Some(owner.into()));
    let saved =
        db::dependency_graph::snapshots(f.pool(), &viewer, repo_id, None, Some(&initial_commit))
            .await
            .unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].detector.name, "Codeyard");
    assert_eq!(saved[0].manifests["Cargo.lock"].resolved.len(), 1);
    db::vulnerabilities::enqueue(f.pool(), repo_id)
        .await
        .unwrap();
    let mut stale = saved[0].clone();
    stale.manifests.clear();
    stale.scanned = chrono::Utc::now();
    db::vulnerabilities::save_snapshot(f.pool(), repo_id, generation, &stale)
        .await
        .unwrap();
    let latest =
        db::dependency_graph::snapshots(f.pool(), &viewer, repo_id, None, Some(&initial_commit))
            .await
            .unwrap();
    assert_eq!(latest[0].manifests["Cargo.lock"].resolved.len(), 1);
    let (status, _, catalogue) = request(
        &router,
        "GET",
        "/api/v3/advisories/GHSA-2345-6789-cfgh",
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{catalogue}");
    assert_github_schema("security-advisories/get-global-advisory", 200, &catalogue);
    assert_eq!(
        catalogue["vulnerabilities"][0]["package"]["ecosystem"],
        "rust"
    );
    let (status, _, listed) = request(&router, "GET", &alerts, Some(reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_github_schema("dependabot/list-alerts-for-repo", 200, &listed);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["state"], "open");
    assert_eq!(listed[0]["security_advisory"]["severity"], "critical");
    let number = listed[0]["number"].as_i64().unwrap();
    let item = format!("{alerts}/{number}");
    for (token, expected) in [(None, 404), (Some(denied.as_str()), 404)] {
        let (status, _, body) = request(&router, "GET", &item, token, Value::Null).await;
        assert_eq!(status.as_u16(), expected, "{body}");
    }
    let (status, _, body) = request(
        &router,
        "PATCH",
        &item,
        Some(reader),
        json!({"state":"dismissed","dismissed_reason":"not_used"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status,_,dismissed)=request(&router,"PATCH",&item,Some(writer),json!({"state":"dismissed","dismissed_reason":"not_used","dismissed_comment":"Not reachable in this application"})).await;
    assert_eq!(status, StatusCode::OK, "{dismissed}");
    assert_github_schema("dependabot/update-alert", 200, &dismissed);
    assert_eq!(dismissed["state"], "dismissed");
    db::vulnerabilities::enqueue(f.pool(), repo_id)
        .await
        .unwrap();
    let (_, generation) = db::vulnerabilities::claim(f.pool()).await.unwrap().unwrap();
    vulnerabilities::scan(&state, &f.repository(repo_id).await, generation)
        .await
        .unwrap();
    let (status, _, detail) = request(&router, "GET", &item, Some(reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_github_schema("dependabot/get-alert", 200, &detail);
    assert_eq!(detail["state"], "dismissed");
    let html = session
        .page_ok("/vuln-org/scan/security/dependabot?state=dismissed")
        .await;
    assert!(html.contains("Reopen alert"), "{html}");
    let number_text = number.to_string();
    let repo_text = repo_id.to_string();
    let reply = session
        .form_reply(
            &format!("/vuln-org/scan/security/dependabot/{number}/dismiss"),
            &[
                ("repository_id", &repo_text),
                ("number", &number_text),
                ("state", "open"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let (status, _, org_alerts) = request(
        &router,
        "GET",
        "/api/v3/orgs/vuln-org/dependabot/alerts",
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{org_alerts}");
    assert_github_schema("dependabot/list-alerts-for-org", 200, &org_alerts);
    assert_eq!(org_alerts.as_array().unwrap().len(), 1);
    let (status, _, org_alerts) = request(
        &router,
        "GET",
        "/api/v3/orgs/vuln-org/dependabot/alerts",
        Some(denied),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{org_alerts}");
    let (status,_,fixed)=request(&router,"PUT",&format!("{base}/contents/Cargo.lock"),Some(writer),json!({"message":"Fix the vulnerable dependency","content":STANDARD.encode(lock("1.2.0")),"sha":blob})).await;
    assert_eq!(status, StatusCode::OK, "{fixed}");
    let (_, generation) = db::vulnerabilities::claim(f.pool()).await.unwrap().unwrap();
    vulnerabilities::scan(&state, &f.repository(repo_id).await, generation)
        .await
        .unwrap();
    let (status, _, detail) = request(&router, "GET", &item, Some(reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["state"], "fixed");
    assert!(detail["fixed_at"].is_string());
    let fixed_commit = fixed["commit"]["sha"].as_str().unwrap();
    let (status, _, changes) = request(
        &router,
        "GET",
        &format!("{base}/dependency-graph/compare/{initial_commit}...{fixed_commit}"),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changes}");
    assert_github_schema("dependency-graph/diff-range", 200, &changes);
    assert_eq!(changes[0]["vulnerabilities"].as_array().unwrap().len(), 1);
    assert_eq!(changes[1]["vulnerabilities"], json!([]));
    // OSV includes vulnerable prereleases that ordinary SemVer requirements exclude.
    let blob = fixed["content"]["sha"].as_str().unwrap();
    let (status, _, vulnerable) = request(&router, "PUT", &format!("{base}/contents/Cargo.lock"), Some(writer),
        json!({"message":"Use a vulnerable prerelease","content":STANDARD.encode(lock("1.2.0-rc.1")),"sha":blob})).await;
    assert_eq!(status, StatusCode::OK, "{vulnerable}");
    let (_, generation) = db::vulnerabilities::claim(f.pool()).await.unwrap().unwrap();
    vulnerabilities::scan(&state, &f.repository(repo_id).await, generation)
        .await
        .unwrap();
    let rows = db::dependabot_alerts::alerts(f.pool(), owner, repo_id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].number, number);
    assert_eq!(rows[0].state, "open");
    assert_eq!(rows[0].version, "1.2.0-rc.1");
    // Older mirror records cannot undo a withdrawal or a newer affected range.
    let mut withdrawn: Value = serde_json::from_slice(fixture).unwrap();
    withdrawn["modified"] = json!("2026-01-03T00:00:00Z");
    withdrawn["withdrawn"] = json!("2026-01-03T00:00:00Z");
    assert!(
        vulnerabilities::ingest(f.pool(), &serde_json::to_vec(&withdrawn).unwrap())
            .await
            .unwrap()
    );
    assert!(!vulnerabilities::ingest(f.pool(), fixture).await.unwrap());
    let (status, _, detail) = request(&router, "GET", &item, Some(reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["state"], "fixed");
    assert_eq!(detail["number"], number);
    let (status, _, catalogue) = request(
        &router,
        "GET",
        "/api/v3/advisories/GHSA-2345-6789-cfgh",
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(catalogue["withdrawn_at"], "2026-01-03T00:00:00Z");
    db::code_security::save(
        f.pool(),
        owner,
        org.id,
        Some(config.id),
        &json!({"dependabot_alerts":"disabled"}),
    )
    .await
    .unwrap();
    let (status, _, body) = request(&router, "GET", &alerts, Some(reader), Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    f.finish().await;
}
#[tokio::test]
async fn sqlite_dependency_vulnerability_alerts() {
    vulnerability_contract(false).await;
}
#[tokio::test]
async fn postgres_dependency_vulnerability_alerts() {
    vulnerability_contract(true).await;
}

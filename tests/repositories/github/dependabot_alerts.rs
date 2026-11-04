use super::{
    assert_github_schema, db, request, tokens, Client, Database, StatusCode, TokenInput, Value,
};

use codeyard_db_core::DbPool;
use serde_json::json;

fn advisory() -> Value {
    json!({"ghsa_id":"GHSA-2345-6789-cfgh","summary":"Unsafe example parser","description":"Upgrade the dependency.","severity":"high","vulnerabilities":[{"package":{"ecosystem":"npm","name":"example"},"vulnerable_version_range":">= 1.0.0, < 2.0.0","first_patched_version":"2.0.0","vulnerable_functions":[]}]})
}
fn snapshot(
    version: &str,
    branch: &str,
    scanned: &str,
) -> codeyard_model::dependency_graph::Snapshot {
    serde_json::from_value(json!({"version":0,"sha":"a".repeat(40),"ref":branch,"scanned":scanned,"job":{"id":"1","correlator":"test"},"detector":{"name":"fixture","version":"1","url":"https://example.test"},"manifests":{"package-lock.json":{"name":"package-lock.json","resolved":{"example":{"package_url":format!("pkg:npm/example@{version}"),"scope":"runtime","relationship":"direct"}}}}})).unwrap()
}
async fn token(f: &Database, client: &Client, actor: i64, scope: &str) -> String {
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
    format!("Bearer {}", token.value)
}
async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("alert-owner").await;
    let other = f.actor("alert-other").await;
    match f.pool() {
        DbPool::Sqlite(p) => {
            sqlx::query("UPDATE users SET is_admin=true WHERE id=$1")
                .bind(owner)
                .execute(p)
                .await
                .unwrap();
        }
        DbPool::Postgres(p) => {
            sqlx::query("UPDATE users SET is_admin=true WHERE id=$1")
                .bind(owner)
                .execute(p)
                .await
                .unwrap();
        }
    }
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let client = Client::login(&router, f.pool(), owner).await;
    let outsider = Client::login(&router, f.pool(), other).await;
    let writer = token(&f, &client, owner, "write").await;
    let reader = token(&f, &client, owner, "read").await;
    let outsider_token = token(&f, &outsider, other, "write").await;
    let (status, _, created) = request(
        &router,
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"dependencies","private":false,"auto_init":true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let repo = created["id"].as_i64().unwrap();
    let branch = format!("refs/heads/{}", created["default_branch"].as_str().unwrap());
    let base = "/api/v3/repos/alert-owner/dependencies/dependabot/alerts";
    for (credentials, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(&reader), StatusCode::FORBIDDEN),
        (Some(&outsider_token), StatusCode::FORBIDDEN),
    ] {
        let (status, _, body) = request(
            &router,
            "POST",
            "/api/v3/advisories",
            credentials.map(String::as_str),
            advisory(),
        )
        .await;
        assert_eq!(status, expected, "{body}");
    }
    let mut bad = advisory();
    bad["ghsa_id"] = json!("bad");
    let (status, _, _) = request(&router, "POST", "/api/v3/advisories", Some(&writer), bad).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _, global) = request(
        &router,
        "POST",
        "/api/v3/advisories",
        Some(&writer),
        advisory(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{global}");
    assert_github_schema("security-advisories/get-global-advisory", 200, &global);
    let id = global["ghsa_id"].as_str().unwrap();
    let (status, _, global) = request(
        &router,
        "GET",
        &format!("/api/v3/advisories/{id}"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_github_schema("security-advisories/get-global-advisory", 200, &global);
    let (status, _, list) = request(
        &router,
        "GET",
        "/api/v3/advisories?ecosystem=npm&affects=example%401.2.0",
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_github_schema("security-advisories/list-global-advisories", 200, &list);
    assert_eq!(list.as_array().unwrap().len(), 1);
    codeyard_db_security::dependency_graph::submit(
        f.pool(),
        owner,
        repo,
        &snapshot("1.2.0", &branch, "2026-01-01T00:00:00Z"),
    )
    .await
    .unwrap();
    codeyard_db_security::dependency_graph::submit(
        f.pool(),
        owner,
        repo,
        &snapshot("3.0.0", "refs/heads/feature", "2026-01-05T00:00:00Z"),
    )
    .await
    .unwrap();
    let (status, _, list) = request(&router, "GET", base, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_github_schema("dependabot/list-alerts-for-repo", 200, &list);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["dependency"]["version_requirement"], "1.2.0");
    let number = list[0]["number"].as_i64().unwrap();
    let path = format!("{base}/{number}");
    let (status, _, body) =
        request(&router, "GET", &path, Some(&outsider_token), Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _, body) = request(&router, "GET", &path, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_github_schema("dependabot/get-alert", 200, &body);
    let (status, _, body) = request(
        &router,
        "PATCH",
        &path,
        Some(&reader),
        json!({"state":"dismissed","dismissed_reason":"not_used"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    for input in [
        json!({"state":"dismissed"}),
        json!({"state":"dismissed","dismissed_reason":"other"}),
        json!({"state":"dismissed","dismissed_reason":"not_used","dismissed_comment":"a".repeat(281)}),
        json!({"state":"open","dismissed_reason":"not_used"}),
    ] {
        let (status, _, body) = request(&router, "PATCH", &path, Some(&writer), input).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    let (status,_,body)=request(&router,"PATCH",&path,Some(&writer),json!({"state":"dismissed","dismissed_reason":"not_used","dismissed_comment":"Only fixtures use this parser"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_github_schema("dependabot/update-alert", 200, &body);
    assert_eq!(body["dismissed_by"]["login"], "alert-owner");
    assert!(!body["dismissed_at"].is_null());
    let native = "/alert-owner/dependencies/security/dependabot";
    let (status, html) = client.page(&format!("{native}?state=dismissed")).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Reopen alert") && html.contains("Only fixtures use this parser"));
    let (status, _) = client
        .form(
            &format!("{native}/{number}/dismiss"),
            &[("repository_id", &repo.to_string()), ("state", "open")],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, body) = request(&router, "GET", &path, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "open");
    assert!(body["dismissed_by"].is_null() && body["dismissed_comment"].is_null());
    let (status, html) = client.page(native).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Unsafe example parser") && html.contains("Import a security advisory"));
    let (status, _) = client
        .form(
            &format!("{native}/import"),
            &[
                ("repository_id", &repo.to_string()),
                ("document", &advisory().to_string()),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _) = client
        .form(
            &format!("{native}/{number}/dismiss"),
            &[
                ("repository_id", &(repo + 1000).to_string()),
                ("state", "dismissed"),
                ("dismissed_reason", "not_used"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = client
        .form(
            &format!("{native}/{number}/dismiss"),
            &[
                ("repository_id", &repo.to_string()),
                ("state", "dismissed"),
                ("dismissed_reason", "tolerable_risk"),
                ("dismissed_comment", "Native form"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, body) = request(
        &router,
        "PATCH",
        &path,
        Some(&writer),
        json!({"state":"open"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["dismissed_reason"].is_null());
    codeyard_db_security::dependency_graph::submit(
        f.pool(),
        owner,
        repo,
        &snapshot("2.0.0", &branch, "2026-01-03T00:00:00Z"),
    )
    .await
    .unwrap();
    let (status, _, body) = request(&router, "GET", &path, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "fixed");
    assert!(!body["fixed_at"].is_null());
    codeyard_db_security::dependency_graph::submit(
        f.pool(),
        owner,
        repo,
        &snapshot("1.0.0", &branch, "2026-01-02T00:00:00Z"),
    )
    .await
    .unwrap();
    assert_eq!(
        codeyard_db_security::dependabot_alerts::alerts(f.pool(), owner, repo)
            .await
            .unwrap()[0]
            .state,
        "fixed"
    );
    codeyard_db_security::dependency_graph::submit(
        f.pool(),
        owner,
        repo,
        &snapshot("1.5.0", &branch, "2026-01-04T00:00:00Z"),
    )
    .await
    .unwrap();
    let rows = codeyard_db_security::dependabot_alerts::alerts(f.pool(), owner, repo)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].number, number);
    assert_eq!(rows[0].state, "open");
    // Published public repository advisories join the same local catalogue, never drafts.
    let repo_advisory = json!({"summary":"Repository advisory","description":"Published locally","severity":"medium","vulnerabilities":[{"package":{"ecosystem":"npm","name":"example"},"vulnerable_version_range":"< 2.0.0","patched_versions":"2.0.0"}]});
    let a = codeyard_db_security::advisories::create(f.pool(), owner, repo, &repo_advisory, false)
        .await
        .unwrap();
    assert!(!codeyard_db_security::dependabot_alerts::global(f.pool())
        .await
        .unwrap()
        .iter()
        .any(|v| v["ghsa_id"] == a.ghsa_id));
    codeyard_db_security::advisories::update(
        f.pool(),
        owner,
        repo,
        &a.ghsa_id,
        &json!({"state":"published"}),
    )
    .await
    .unwrap();
    assert!(codeyard_db_security::dependabot_alerts::global(f.pool())
        .await
        .unwrap()
        .iter()
        .any(|v| v["ghsa_id"] == a.ghsa_id));
    assert_eq!(
        codeyard_db_security::dependabot_alerts::alerts(f.pool(), owner, repo)
            .await
            .unwrap()
            .len(),
        2
    );
    let (status, headers, page) = request(
        &router,
        "GET",
        &format!("{base}?per_page=1"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page.as_array().unwrap().len(), 1);
    let link = headers["link"].to_str().unwrap();
    let next = link.split('<').nth(1).unwrap().split('>').next().unwrap();
    let next = url::Url::parse(next).unwrap();
    let (status, _, second) = request(
        &router,
        "GET",
        &format!("{}?{}", next.path(), next.query().unwrap()),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_ne!(page[0]["number"], second[0]["number"]);
    use codeyard_db_orgs::organizations;
    use codeyard_db_orgs::organizations::profiles::NewOrganization;
    use codeyard_db_orgs::organizations::profiles::OrganizationProfile;
    let created = organizations::create_organization(
        f.pool(),
        owner,
        &NewOrganization::new("alert-org", OrganizationProfile::new(None, None).unwrap()).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        created,
        organizations::OrganizationOutcome::Applied(_)
    ));
    let (status, _, org_repo) = request(
        &router,
        "POST",
        "/api/v3/orgs/alert-org/repos",
        Some(&writer),
        json!({"name":"project","private":true,"auto_init":true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{org_repo}");
    let org_repo_id = org_repo["id"].as_i64().unwrap();
    // A private repository's graph is on once Dependabot alerts are, as on GitHub.
    let (status, _, body) = request(
        &router,
        "PUT",
        "/api/v3/repos/alert-org/project/vulnerability-alerts",
        Some(&writer),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let org_branch = format!(
        "refs/heads/{}",
        org_repo["default_branch"].as_str().unwrap()
    );
    codeyard_db_security::dependency_graph::submit(
        f.pool(),
        owner,
        org_repo_id,
        &snapshot("1.0.0", &org_branch, "2026-01-05T00:00:00Z"),
    )
    .await
    .unwrap();
    let (status, _, body) = request(
        &router,
        "GET",
        "/api/v3/orgs/alert-org/dependabot/alerts?package=example",
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_github_schema("dependabot/list-alerts-for-org", 200, &body);
    assert_eq!(body.as_array().unwrap().len(), 2);
    let private = codeyard_db_security::advisories::create(
        f.pool(),
        owner,
        org_repo_id,
        &repo_advisory,
        false,
    )
    .await
    .unwrap();
    codeyard_db_security::advisories::update(
        f.pool(),
        owner,
        org_repo_id,
        &private.ghsa_id,
        &json!({"state":"published"}),
    )
    .await
    .unwrap();
    assert!(!codeyard_db_security::dependabot_alerts::global(f.pool())
        .await
        .unwrap()
        .iter()
        .any(|v| v["ghsa_id"] == private.ghsa_id));
    for path in [
        "/api/v3/enterprises/absent/dependabot/alerts",
        "/api/v3/enterprises/absent/dependabot/repository-access",
    ] {
        let (status, _, _) = request(&router, "GET", path, Some(&writer), Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let mut withdrawn = advisory();
    withdrawn["withdrawn_at"] = json!("2026-02-01T00:00:00Z");
    codeyard_db_security::dependabot_alerts::import(f.pool(), owner, &withdrawn)
        .await
        .unwrap();
    let rows = codeyard_db_security::dependabot_alerts::alerts(f.pool(), owner, repo)
        .await
        .unwrap();
    assert_eq!(
        rows.iter().find(|r| r.ghsa_id == id).unwrap().state,
        "fixed"
    );
    f.finish().await;
}
#[tokio::test]
async fn dependabot_alerts_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn dependabot_alerts_postgres() {
    contract(true).await;
}

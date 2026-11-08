use super::assert_github_schema;
use super::request;
use super::Client;
use super::Database;
use super::StatusCode;
use super::TokenInput;
use super::Value;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use codeyard_identity::tokens;
use serde_json::json;
use tower::ServiceExt;
async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("advisory-owner").await;
    let reporter = f.actor("advisory-reporter").await;
    let stranger = f.actor("advisory-stranger").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let owner_client = Client::login(&router, f.pool(), owner).await;
    let reporter_client = Client::login(&router, f.pool(), reporter).await;
    let stranger_client = Client::login(&router, f.pool(), stranger).await;
    let token = tokens::create(
        f.pool(),
        owner,
        Some(&owner_client.session),
        TokenInput {
            name: "advisories",
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
        Some(&owner_client.session),
        TokenInput {
            name: "advisories read",
            scope: "read",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let reader = format!("Bearer {}", token.value);
    let token = tokens::create(
        f.pool(),
        reporter,
        Some(&reporter_client.session),
        TokenInput {
            name: "reporter",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let reporting = format!("Bearer {}", token.value);
    let (status, _, repo) = request(
        &router,
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"disclosures","private":false,"auto_init":true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{repo}");
    let repo_id = repo["id"].as_i64().unwrap();
    let base = "/api/v3/repos/advisory-owner/disclosures/security-advisories";
    let draft = json!({"summary":"A confidential finding","description":"Details remain private until explicit publication.","severity":"high","vulnerabilities":[{"package":{"ecosystem":"npm","name":"example"},"vulnerable_version_range":"< 1.0.1","patched_versions":"1.0.1"}],"cwe_ids":["CWE-79"]});
    let (status, _, _) = request(&router, "POST", base, Some(&reader), draft.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = request(&router, "POST", base, Some(&reporting), draft.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, created) = request(&router, "POST", base, Some(&writer), draft.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_github_schema(
        "security-advisories/create-repository-advisory",
        201,
        &created,
    );
    assert_eq!(created["state"], "draft");
    let ghsa = created["ghsa_id"].as_str().unwrap();
    let path = format!("{base}/{ghsa}");
    let (status, _, _) = request(&router, "GET", &path, None, Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, empty) = request(&router, "GET", base, None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty, json!([]));
    let (status, _, detail) = request(&router, "GET", &path, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_github_schema("security-advisories/get-repository-advisory", 200, &detail);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{path}/cve"))
                .header("authorization", &writer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(to_bytes(response.into_body(), 1024)
        .await
        .unwrap()
        .is_empty());
    let (status, _, _) = request(
        &router,
        "POST",
        &format!("{path}/cve"),
        Some(&writer),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _, detail) = request(&router, "GET", &path, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail["cve_id"],
        Value::Null,
        "A local request must not fabricate a registered identifier"
    );
    let native = format!("/advisory-owner/disclosures/security/advisories/{ghsa}");
    let (status, html) = owner_client.page(&native).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("No CVE registrar is connected"));
    assert!(!html.contains("Record CVE request"));
    let (status, _, fork) = request(
        &router,
        "POST",
        &format!("{path}/forks"),
        Some(&writer),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{fork}");
    assert_github_schema("security-advisories/create-fork", 202, &fork);
    assert_eq!(fork["private"], true);
    assert_eq!(fork["fork"], true);
    let fork_api = format!("/api/v3/repos/{}", fork["full_name"].as_str().unwrap());
    let (status, _, _) = request(
        &router,
        "GET",
        &format!(
            "{fork_api}/commits/{}",
            repo["default_branch"].as_str().unwrap()
        ),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "The remediation fork must contain Git objects"
    );
    let (status, _, _) = request(&router, "GET", &fork_api, None, Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The GitHub repository-update adapter is outside this lane; exercise the
    // implemented native settings adapter and its transactional DAL instead.
    let (status, body) = owner_client
        .send(
            "PATCH",
            &format!("/api/repos/{}", fork["full_name"].as_str().unwrap()),
            "application/json",
            json!({"is_private":false}).to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let fork_id = fork["id"].as_i64().unwrap();
    let error = codeyard_db_repos::repositories::records::update_repository(
        f.pool(),
        owner,
        fork_id,
        None,
        false,
        codeyard_db_repos::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None,
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error.downcast_ref::<codeyard_db_repos::repositories::records::RepositorySettingsRefusal>(),
            Some(codeyard_db_repos::repositories::records::RepositorySettingsRefusal::VisibilityLocked)
        ),
        "{error:?}"
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repositories WHERE id=$1 AND is_private AND visibility_locked",
            &[fork_id]
        )
        .await,
        1
    );
    let (status,_,updated)=request(&router,"PATCH",&path,Some(&writer),json!({"state":"published","severity":null,"cvss_vector_string":"CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"})).await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_github_schema(
        "security-advisories/update-repository-advisory",
        200,
        &updated,
    );
    assert_eq!(updated["severity"], "critical");
    assert_eq!(updated["cvss"]["score"], 9.8);
    let (status, _, public) = request(&router, "GET", &path, None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(public["private_fork"], Value::Null);
    let (status, _, _) = request(
        &router,
        "PATCH",
        &path,
        Some(&writer),
        json!({"state":"draft"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "Published disclosures cannot silently become private again"
    );
    let reports = format!("{base}/reports");
    let report = json!({"summary":"Private report with unique confidential marker","description":"Only the reporter and current administrators may read this.","vulnerabilities":[]});
    let (status, _, _) = request(&router, "POST", &reports, Some(&reporting), report.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let enable = "/api/v3/repos/advisory-owner/disclosures/private-vulnerability-reporting";
    let (status, _, _) = request(&router, "PUT", enable, Some(&writer), Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, reported) =
        request(&router, "POST", &reports, Some(&reporting), report.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{reported}");
    assert_github_schema(
        "security-advisories/create-private-vulnerability-report",
        201,
        &reported,
    );
    assert_eq!(reported["state"], "triage");
    assert_eq!(reported["submission"]["accepted"], false);
    let report_ghsa = reported["ghsa_id"].as_str().unwrap();
    let report_api = format!("{base}/{report_ghsa}");
    let (status, _, body) = request(
        &router,
        "PATCH",
        &report_api,
        Some(&writer),
        json!({"state":"triage"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _, _) = request(&router, "GET", &report_api, None, Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = request(&router, "GET", &report_api, Some(&reporting), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let record = codeyard_db_security::advisories::get(f.pool(), Some(owner), repo_id, report_ghsa)
        .await
        .unwrap()
        .unwrap();
    let report_id = record.report_id.unwrap();
    let thread = format!("/advisory-owner/disclosures/security/reports/{report_id}");
    let (status, html) = reporter_client.page(&thread).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Only the reporter"));
    let (status, _) = stranger_client.page(&thread).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, updated) = request(
        &router,
        "PATCH",
        &report_api,
        Some(&writer),
        json!({"state":"draft"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["submission"]["accepted"], true);
    let (status, html) = owner_client
        .page(&format!(
            "/advisory-owner/disclosures/security/advisories/{report_ghsa}"
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains("Package name"),
        "Reports without package metadata must be completable natively"
    );
    let repository_identity = repo_id.to_string();
    let (status, html) = owner_client
        .form(
            "/advisory-owner/disclosures/_forms/advisory",
            &[
                ("op", "save"),
                ("ghsa", report_ghsa),
                ("repository_id", &repository_identity),
                ("summary", reported["summary"].as_str().unwrap()),
                ("description", reported["description"].as_str().unwrap()),
                ("severity", "medium"),
                ("vector", ""),
                ("cve", ""),
                ("ecosystem", "npm"),
                ("package", "reported-native-package"),
                ("range", "<2.0.0"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
    let (status, _, completed) =
        request(&router, "GET", &report_api, Some(&writer), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_github_schema(
        "security-advisories/get-repository-advisory",
        200,
        &completed,
    );
    assert_eq!(completed["state"], "draft");
    assert_eq!(
        completed["vulnerabilities"][0]["package"]["name"],
        "reported-native-package"
    );
    for _ in 0..4 {
        let (status, _, body) =
            request(&router, "POST", &reports, Some(&reporting), report.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, _, _) = request(&router, "POST", &reports, Some(&reporting), report).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM issues WHERE repo_id=$1", &[repo_id])
            .await,
        0,
        "Confidential reports are never ordinary issues"
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repository_security_reports WHERE repo_id=$1",
            &[repo_id]
        )
        .await,
        5
    );
    let (status, headers, list) = request(
        &router,
        "GET",
        &format!("{base}?per_page=2"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_github_schema("security-advisories/list-repository-advisories", 200, &list);
    assert_eq!(list.as_array().unwrap().len(), 2);
    let link = headers["link"].to_str().unwrap();
    let next = link.split('<').nth(1).unwrap().split('>').next().unwrap();
    let url = url::Url::parse(next).unwrap();
    let (status, _, second) = request(
        &router,
        "GET",
        &format!("{}?{}", url.path(), url.query().unwrap()),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second.as_array().unwrap().len(), 2);
    assert_ne!(second[0]["ghsa_id"], list[0]["ghsa_id"]);
    let input = codeyard_db_orgs::organizations::profiles::NewOrganization::new(
        "advisory-org",
        codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(
            Some("Advisory organization"),
            None,
        )
        .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        codeyard_db_orgs::organizations::profiles::create_organization(f.pool(), owner, &input)
            .await
            .unwrap(),
        codeyard_database::queries::organizations::OrganizationOutcome::Applied(_)
    ));
    for private in [false, true] {
        let name = if private { "private" } else { "public" };
        let (status, _, repo) = request(
            &router,
            "POST",
            "/api/v3/orgs/advisory-org/repos",
            Some(&writer),
            json!({"name":name,"private":private}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{repo}");
        let base = format!("/api/v3/repos/advisory-org/{name}/security-advisories");
        let (status, _, created) =
            request(&router, "POST", &base, Some(&writer), draft.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let (status, _, body) = request(
            &router,
            "PATCH",
            &format!("{base}/{}", created["ghsa_id"].as_str().unwrap()),
            Some(&writer),
            json!({"state":"published"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    for (auth, count) in [(None, 1), (Some(reader.as_str()), 2)] {
        let (status, _, list) = request(
            &router,
            "GET",
            "/api/v3/orgs/advisory-org/security-advisories",
            auth,
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{list}");
        assert_github_schema(
            "security-advisories/list-org-repository-advisories",
            200,
            &list,
        );
        assert_eq!(list.as_array().unwrap().len(), count);
    }
    let (status, html) = stranger_client
        .page(&format!(
            "/advisory-owner/disclosures/security/advisories/{report_ghsa}"
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!html.contains("unique confidential marker"));
    let native_report = owner_client
        .form_reply(
            "/advisory-owner/disclosures/security",
            &[
                ("repository_id", &repository_identity),
                ("title", "Native private report"),
                (
                    "body",
                    "Native and REST reports share one confidential triage workflow.",
                ),
            ],
        )
        .await;
    assert_eq!(
        native_report.status,
        StatusCode::SEE_OTHER,
        "{}",
        native_report.text()
    );
    let (status, html) = owner_client
        .page(native_report.location.as_deref().unwrap())
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Native and REST reports share"));
    assert_eq!(f.scalar("SELECT COUNT(*) FROM repository_advisories a JOIN repository_security_reports s ON s.id=a.report_id WHERE a.repo_id=$1 AND a.state='triage' AND a.author_id=s.reporter_id AND s.title='Native private report'",&[repo_id]).await,1);
    f.execute("UPDATE users SET is_banned=true WHERE id=$1", &[reporter])
        .await;
    assert!(
        codeyard_db_security::advisories::get(f.pool(), Some(reporter), repo_id, report_ghsa)
            .await
            .unwrap()
            .is_none(),
        "Confidential reads use current account authority, not a stale authenticated actor"
    );
    let (status, _, _) = request(&router, "GET", &report_api, Some(&reporting), Value::Null).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    f.execute(
        "UPDATE users SET is_banned=false,must_change_password=true WHERE id=$1",
        &[reporter],
    )
    .await;
    assert!(
        codeyard_db_security::advisories::get(f.pool(), Some(reporter), repo_id, report_ghsa)
            .await
            .unwrap()
            .is_none()
    );
    f.finish().await;
}
#[tokio::test]
async fn sqlite_advisories_http_contract() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_advisories_http_contract() {
    contract(true).await;
}

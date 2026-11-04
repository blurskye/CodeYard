use super::assert_github_schema;
use super::request;
use super::Client;
use super::Database;
use super::StatusCode;
use super::TokenInput;
use super::Value;
use serde_json::json;

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("pages-owner").await;
    let outsider = f.actor("pages-outsider").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let client = Client::login(&router, f.pool(), owner).await;
    let stranger = Client::login(&router, f.pool(), outsider).await;
    let token = tokens::create(
        f.pool(),
        owner,
        Some(&client.session),
        TokenInput {
            name: "pages",
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
        Some(&client.session),
        TokenInput {
            name: "pages read",
            scope: "read",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let reader = format!("Bearer {}", token.value);
    let token = tokens::create(
        f.pool(),
        outsider,
        Some(&stranger.session),
        TokenInput {
            name: "pages outsider",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let other = format!("Bearer {}", token.value);
    let (status, _, repo) = request(
        &router,
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"site","private":true,"auto_init":true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{repo}");
    let repo_id = repo["id"].as_i64().unwrap();
    let base = "/api/v3/repos/pages-owner/site/pages";
    let (status, _, _) = request(&router, "GET", base, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let config = json!({"source":{"branch":repo["default_branch"]}});
    let (status, _, _) = request(&router, "POST", base, Some(&reader), config.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, site) = request(&router, "POST", base, Some(&writer), config.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{site}");
    assert_github_schema("repos/create-pages-site", 201, &site);
    assert_eq!(site["status"], Value::Null);
    assert!(
        site.get("html_url").is_none(),
        "No hosted URL is advertised before a successful deployment"
    );
    let (status, _, _) = request(&router, "POST", base, Some(&writer), config).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    for input in [
        json!({"source":{"branch":"missing","path":"/"}}),
        json!({"source":{"branch":"main","path":"/private"}}),
        json!({"cname":"https://127.0.0.1"}),
        json!({"source":{"branch":"main","path":true}}),
        json!({"build_type":"unknown"}),
    ] {
        let (status, _, body) = request(&router, "PUT", base, Some(&writer), input).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    for auth in [None, Some(other.as_str())] {
        let (status, _, _) = request(&router, "GET", base, auth, Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (status, _, site) = request(&router, "GET", base, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_github_schema("repos/get-pages", 200, &site);
    let builds = format!("{base}/builds");
    let (status, _, empty) = request(&router, "GET", &builds, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty, json!([]));
    let (status, _, _) = request(
        &router,
        "GET",
        &format!("{builds}/latest"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, created) = request(&router, "POST", &builds, Some(&writer), Value::Null).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_github_schema("repos/request-pages-build", 201, &created);
    assert_eq!(
        created["status"], "errored",
        "An unavailable publisher must never claim success or leave work queued"
    );
    let build_url = url::Url::parse(created["url"].as_str().unwrap()).unwrap();
    let (status, _, first) =
        request(&router, "GET", build_url.path(), Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_github_schema("repos/get-pages-build", 200, &first);
    assert!(first["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not configured"));
    let (status, _, latest) = request(
        &router,
        "GET",
        &format!("{builds}/latest"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_github_schema("repos/get-latest-pages-build", 200, &latest);
    assert_eq!(latest, first);
    let (status, _, _) = request(&router, "POST", &builds, Some(&writer), Value::Null).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, headers, list) = request(
        &router,
        "GET",
        &format!("{builds}?per_page=1"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_github_schema("repos/list-pages-builds", 200, &list);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let native = "/pages-owner/site/settings/pages";
    let (status, html) = client.page(native).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("does not host Pages sites"));
    let (status, _) = client
        .form(
            "/pages-owner/site/_forms/pages",
            &[
                ("repository_id", &repo_id.to_string()),
                ("op", "save"),
                ("build_type", "workflow"),
                ("cname", "pages.invalid"),
                ("https_enforced", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, health) = request(
        &router,
        "GET",
        &format!("{base}/health"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{health}");
    assert_github_schema("repos/get-pages-health-check", 200, &health);
    assert_eq!(health["domain"]["dns_resolves"], false);
    assert_eq!(health["domain"]["is_served_by_pages"], false);
    let deployments = format!("{base}/deployments");
    let input = json!({"artifact_url":"https://example.invalid/artifact.zip","pages_build_version":first["commit"],"oidc_token":"This credential must never be persisted or trusted"});
    let (status, _, created) = request(&router, "POST", &deployments, Some(&writer), input).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_github_schema("repos/create-pages-deployment", 200, &created);
    let deployment_url = url::Url::parse(created["status_url"].as_str().unwrap()).unwrap();
    let (status, _, deployment) = request(
        &router,
        "GET",
        deployment_url.path(),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_github_schema("repos/get-pages-deployment", 200, &deployment);
    assert_eq!(deployment["status"], "deployment_failed");
    let cancel = format!("{}/cancel", deployment_url.path());
    let (status, _, _) = request(&router, "POST", &cancel, Some(&writer), Value::Null).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Terminal deployments cannot be cancelled"
    );
    let id = created["id"].as_str().unwrap().parse::<i64>().unwrap();
    f.execute(
        "UPDATE pages_deployments SET status='deployment_in_progress' WHERE id=$1",
        &[id],
    )
    .await;
    let (status, _, _) = request(&router, "POST", &cancel, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = request(&router, "POST", &cancel, Some(&writer), Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, deployment) = request(
        &router,
        "GET",
        deployment_url.path(),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deployment["status"], "deployment_cancelled");
    let (status, _, _) = request(
        &router,
        "PUT",
        base,
        Some(&writer),
        json!({"cname":null,"https_enforced":false}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, _, site) = request(&router, "GET", base, Some(&reader), Value::Null).await;
    assert_eq!(site["cname"], Value::Null);
    let (status, _, _) = request(&router, "DELETE", base, Some(&writer), Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = request(&router, "GET", base, Some(&reader), Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM pages_builds WHERE repo_id=$1",
            &[repo_id]
        )
        .await,
        2,
        "Deleting settings retains the build audit history"
    );
    let (status, _, public_repo) = request(
        &router,
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"public-site","private":false,"auto_init":true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{public_repo}");
    let (status, _, site) = request(
        &router,
        "POST",
        "/api/v3/repos/pages-owner/public-site/pages",
        Some(&writer),
        json!({"source":{"branch":public_repo["default_branch"]}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{site}");
    assert_eq!(site["public"], true);
    let (status, _) = client
        .form(
            "/pages-owner/public-site/_forms/pages",
            &[("repository_id", &repo_id.to_string()), ("op", "delete")],
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "Native forms stay bound to the repository they were rendered for"
    );
    f.finish().await;
}
#[tokio::test]
async fn sqlite_pages_http_contract() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_pages_http_contract() {
    contract(true).await;
}

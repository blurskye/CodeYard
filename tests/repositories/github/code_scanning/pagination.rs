use super::call;
use super::checked;
use super::encode_sarif;
use super::json;
use super::sarif_document;
use super::Client;
use super::Router;
use super::StatusCode;
use super::Value;

async fn remove_upload(router: &Router, token: &str, upload: &str) {
    let analyses = call(
        router,
        "GET",
        &format!("/api/v3/repos/alice/scan/code-scanning/analyses?sarif_id={upload}"),
        token,
        Value::Null,
        "code-scanning/list-recent-analyses",
        200,
    )
    .await;
    assert_eq!(analyses.as_array().unwrap().len(), 1);
    let id = analyses[0]["id"].as_i64().unwrap();
    call(
        router,
        "DELETE",
        &format!("/api/v3/repos/alice/scan/code-scanning/analyses/{id}?confirm_delete=true"),
        token,
        Value::Null,
        "code-scanning/delete-analysis",
        200,
    )
    .await;
}

pub(super) async fn contract(
    router: &Router,
    pool: &codeyard_database::DbPool,
    actor: i64,
    token: &str,
    sha: &str,
) {
    let endpoint = "/api/v3/repos/alice/scan/code-scanning";
    let before = call(
        router,
        "GET",
        &format!("{endpoint}/alerts/1"),
        token,
        Value::Null,
        "code-scanning/get-alert",
        200,
    )
    .await;
    let mut report = sarif_document("pagination/", true, 1);
    let template = report["runs"][0]["results"][0].clone();
    report["runs"][0]["results"] = json!((0..31)
        .map(|index| {
            let mut result = template.clone();
            result["partialFingerprints"]["primaryLocationLineHash"] =
                json!(format!("pagination-{index}"));
            result
        })
        .collect::<Vec<_>>());
    let upload = call(
        router,
        "POST",
        &format!("{endpoint}/sarifs"),
        token,
        json!({"commit_sha":sha,"ref":"refs/heads/main","sarif":encode_sarif(report)}),
        "code-scanning/upload-sarif",
        202,
    )
    .await;
    assert!(codeyard_repos_pulls::code_scanning::process_next(pool)
        .await
        .unwrap());
    let status = call(
        router,
        "GET",
        &format!("{endpoint}/sarifs/{}", upload["id"].as_str().unwrap()),
        token,
        Value::Null,
        "code-scanning/get-sarif",
        200,
    )
    .await;
    assert_eq!(status["processing_status"], "complete");

    let (headers, first) = checked(
        router,
        "code-scanning/list-alerts-for-repo",
        "GET",
        &format!("{endpoint}/alerts?per_page=10"),
        Some(token),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(first.as_array().unwrap().len(), 10);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let second = call(
        router,
        "GET",
        &format!("{endpoint}/alerts?per_page=10&page=2"),
        token,
        Value::Null,
        "code-scanning/list-alerts-for-repo",
        200,
    )
    .await;
    assert_eq!(second.as_array().unwrap().len(), 10);
    for alert in first.as_array().unwrap() {
        assert!(!second
            .as_array()
            .unwrap()
            .iter()
            .any(|next| next["number"] == alert["number"]));
    }
    let highest = first[0]["number"].as_i64().unwrap();
    let oldest = call(
        router,
        "GET",
        &format!("{endpoint}/alerts?per_page=1&direction=asc&ref=main"),
        token,
        Value::Null,
        "code-scanning/list-alerts-for-repo",
        200,
    )
    .await;
    assert_eq!(oldest[0]["number"], 1);
    let after = call(
        router,
        "GET",
        &format!("{endpoint}/alerts/1"),
        token,
        Value::Null,
        "code-scanning/get-alert",
        200,
    )
    .await;
    assert_eq!(
        after["updated_at"], before["updated_at"],
        "an unrelated analysis must not retimestamp existing alerts"
    );

    for query in ["state=invalid", "sort=invalid", "direction=invalid"] {
        call(
            router,
            "GET",
            &format!("{endpoint}/alerts?{query}"),
            token,
            Value::Null,
            "code-scanning/list-alerts-for-repo",
            422,
        )
        .await;
    }
    call(
        router,
        "GET",
        &format!("{endpoint}/analyses?sort=updated"),
        token,
        Value::Null,
        "code-scanning/list-recent-analyses",
        422,
    )
    .await;
    let analyses = call(
        router,
        "GET",
        &format!("{endpoint}/analyses?per_page=1&direction=asc"),
        token,
        Value::Null,
        "code-scanning/list-recent-analyses",
        200,
    )
    .await;
    assert_eq!(analyses.as_array().unwrap().len(), 1);
    let pr = call(
        router,
        "GET",
        &format!("{endpoint}/analyses?pr=1"),
        token,
        Value::Null,
        "code-scanning/list-recent-analyses",
        200,
    )
    .await;
    assert!(!pr.as_array().unwrap().is_empty());
    assert!(pr.as_array().unwrap().iter().all(|analysis| analysis["ref"]
        .as_str()
        .unwrap()
        .starts_with("refs/pull/1/")));
    let instances = call(
        router,
        "GET",
        &format!("{endpoint}/alerts/1/instances?per_page=1&ref=main"),
        token,
        Value::Null,
        "code-scanning/list-alert-instances",
        200,
    )
    .await;
    assert_eq!(instances.as_array().unwrap().len(), 1);
    assert_eq!(instances[0]["ref"], "refs/heads/main");

    let client = Client::login(router, pool, actor).await;
    let html = client
        .exchange(
            "GET",
            "/alice/scan/security/code-scanning",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(html.status, StatusCode::OK);
    assert!(html
        .text()
        .contains("/alice/scan/security/code-scanning?page=2"));
    let html = client
        .exchange(
            "GET",
            "/alice/scan/security/code-scanning?page=2",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(html.status, StatusCode::OK);
    assert!(html.text().contains("Previous"));

    remove_upload(router, token, upload["id"].as_str().unwrap()).await;
    let mut replacement = sarif_document("pagination/", true, 1);
    replacement["runs"][0]["results"][0]["partialFingerprints"]["primaryLocationLineHash"] =
        json!("pagination-replacement");
    let upload = call(
        router,
        "POST",
        &format!("{endpoint}/sarifs"),
        token,
        json!({"commit_sha":sha,"ref":"refs/heads/main","sarif":encode_sarif(replacement)}),
        "code-scanning/upload-sarif",
        202,
    )
    .await;
    assert!(codeyard_repos_pulls::code_scanning::process_next(pool)
        .await
        .unwrap());
    let alerts = call(
        router,
        "GET",
        &format!("{endpoint}/alerts"),
        token,
        Value::Null,
        "code-scanning/list-alerts-for-repo",
        200,
    )
    .await;
    assert!(
        alerts[0]["number"].as_i64().unwrap() > highest,
        "deleting analyses must not recycle public alert numbers"
    );
    remove_upload(router, token, upload["id"].as_str().unwrap()).await;
}

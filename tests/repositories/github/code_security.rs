mod transfers;

use super::checked;
use super::code_scanning::call;
use super::json;
use super::Router;
use super::StatusCode;
use super::Value;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use tower::ServiceExt;

async fn accepted(router: &Router, path: &str, writer: &str, input: Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", writer)
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(to_bytes(response.into_body(), 1024)
        .await
        .unwrap()
        .is_empty());
}
pub(super) async fn contract(
    router: &Router,
    pool: &codeyard_db_core::DbPool,
    repo: i64,
    actor: i64,
    writer: &str,
    reader: &str,
    outsider: &str,
) {
    let base = "/api/v3/orgs/acme/code-security/configurations";
    let repo_config = "/api/v3/repos/acme/scan/code-security-configuration";
    call(
        router,
        "GET",
        repo_config,
        reader,
        Value::Null,
        "code-security/get-configuration-for-repository",
        204,
    )
    .await;
    let first=call(router,"POST",base,writer,json!({"name":"Standard security","description":"Repository defaults","secret_scanning":"enabled","secret_scanning_push_protection":"enabled","dependabot_alerts":"enabled","code_scanning_default_setup":"disabled"}),"code-security/create-configuration",201).await;
    let id = first["id"].as_i64().unwrap();
    let item = format!("{base}/{id}");
    let other = call(
        router,
        "POST",
        base,
        writer,
        json!({"name":"Other"}),
        "code-security/create-configuration",
        201,
    )
    .await;
    call(
        router,
        "POST",
        base,
        writer,
        json!({"name":"Standard security"}),
        "code-security/create-configuration",
        422,
    )
    .await;
    call(
        router,
        "POST",
        base,
        reader,
        json!({"name":"Read only"}),
        "code-security/create-configuration",
        403,
    )
    .await;
    call(
        router,
        "POST",
        base,
        outsider,
        json!({"name":"Denied"}),
        "code-security/create-configuration",
        403,
    )
    .await;
    call(
        router,
        "POST",
        base,
        writer,
        json!({"name":"Invalid","secret_scanning":"yes"}),
        "code-security/create-configuration",
        422,
    )
    .await;
    call(
        router,
        "POST",
        base,
        writer,
        json!({"name":"No hosted CodeQL","code_scanning_default_setup":"enabled"}),
        "code-security/create-configuration",
        422,
    )
    .await;
    let (headers, list) = checked(
        router,
        "code-security/get-configurations-for-org",
        "GET",
        &format!("{base}?per_page=1"),
        Some(reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(headers["link"]
        .to_str()
        .unwrap()
        .contains("/api/v3/orgs/acme/"));
    call(
        router,
        "GET",
        base,
        outsider,
        Value::Null,
        "code-security/get-configurations-for-org",
        403,
    )
    .await;
    call(
        router,
        "GET",
        &item,
        reader,
        Value::Null,
        "code-security/get-configuration",
        200,
    )
    .await;
    call(
        router,
        "PATCH",
        &item,
        writer,
        json!({"description":"Updated","enforcement":"unenforced","private_vulnerability_reporting":"enabled"}),
        "code-security/update-configuration",
        200,
    )
    .await;
    call(
        router,
        "PUT",
        &format!("{item}/defaults"),
        writer,
        json!({"default_for_new_repos":"public"}),
        "code-security/set-configuration-as-default",
        200,
    )
    .await;
    let defaults = call(
        router,
        "GET",
        &format!("{base}/defaults"),
        reader,
        Value::Null,
        "code-security/get-default-configurations",
        200,
    )
    .await;
    assert_eq!(defaults.as_array().unwrap().len(), 1);
    call(
        router,
        "PUT",
        &format!("{base}/{}/defaults", other["id"].as_i64().unwrap()),
        writer,
        json!({"default_for_new_repos":"private_and_internal"}),
        "code-security/set-configuration-as-default",
        200,
    )
    .await;
    let defaults = call(
        router,
        "GET",
        &format!("{base}/defaults"),
        reader,
        Value::Null,
        "code-security/get-default-configurations",
        200,
    )
    .await;
    assert_eq!(defaults.as_array().unwrap().len(), 2);
    call(
        router,
        "PUT",
        &format!("{item}/defaults"),
        writer,
        json!({"default_for_new_repos":"all"}),
        "code-security/set-configuration-as-default",
        200,
    )
    .await;
    let defaults = call(
        router,
        "GET",
        &format!("{base}/defaults"),
        reader,
        Value::Null,
        "code-security/get-default-configurations",
        200,
    )
    .await;
    assert_eq!(defaults.as_array().unwrap().len(), 1);
    let new_repo = call(
        router,
        "POST",
        "/api/v3/orgs/acme/repos",
        writer,
        json!({"name":"default-security"}),
        "repos/create-in-org",
        201,
    )
    .await;
    assert_eq!(
        new_repo["security_and_analysis"]["secret_scanning"]["status"],
        "enabled"
    );
    let settings =
        codeyard_db_security::secret_scanning::settings(pool, new_repo["id"].as_i64().unwrap())
            .await
            .unwrap();
    assert!(settings.enabled && settings.push_protection);
    // The configuration also turns on private vulnerability reporting for the public repository.
    assert!(codeyard_db_repos::repositories::security::enabled(
        pool,
        new_repo["id"].as_i64().unwrap()
    )
    .await
    .unwrap());
    let linked = call(
        router,
        "GET",
        "/api/v3/repos/acme/default-security/code-security-configuration",
        reader,
        Value::Null,
        "code-security/get-configuration-for-repository",
        200,
    )
    .await;
    assert_eq!(linked["configuration"]["id"], id);
    accepted(
        router,
        &format!("{item}/attach"),
        writer,
        json!({"scope":"selected","selected_repository_ids":[repo]}),
    )
    .await;
    let linked = call(
        router,
        "GET",
        repo_config,
        reader,
        Value::Null,
        "code-security/get-configuration-for-repository",
        200,
    )
    .await;
    assert_eq!(linked["status"], "attached");
    let repos = call(
        router,
        "GET",
        &format!("{item}/repositories"),
        reader,
        Value::Null,
        "code-security/get-repositories-for-configuration",
        200,
    )
    .await;
    assert_eq!(repos.as_array().unwrap().len(), 2);
    let org = codeyard_db_orgs::organizations::profiles::get_public_organization(pool, "acme")
        .await
        .unwrap()
        .unwrap();
    let session = super::Client::login(router, pool, actor).await;
    let page_path = format!("/settings/organizations/{}/security", org.id);
    let page = session
        .exchange("GET", &page_path, "text/html", Vec::new())
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text().contains("Standard security"));
    assert!(page.text().contains("Code security configurations"));
    let missing_csrf = session
        .exchange(
            "POST",
            &format!("{page_path}/save"),
            "application/x-www-form-urlencoded",
            b"name=Native".to_vec(),
        )
        .await;
    assert_eq!(missing_csrf.status, StatusCode::FORBIDDEN);
    let native = session
        .form_reply(
            &format!("{page_path}/save"),
            &[
                ("name", "Native configuration"),
                ("description", "Managed without JavaScript"),
                ("secret_scanning", "enabled"),
            ],
        )
        .await;
    assert_eq!(native.status, StatusCode::SEE_OTHER);
    let list = call(
        router,
        "GET",
        base,
        reader,
        Value::Null,
        "code-security/get-configurations-for-org",
        200,
    )
    .await;
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["name"] == "Native configuration"));
    call(
        router,
        "PATCH",
        &item,
        writer,
        json!({"secret_scanning_push_protection":"disabled"}),
        "code-security/update-configuration",
        200,
    )
    .await;
    let repo_document = call(
        router,
        "GET",
        "/api/v3/repos/acme/scan",
        reader,
        Value::Null,
        "repos/get",
        200,
    )
    .await;
    assert_eq!(
        repo_document["security_and_analysis"]["secret_scanning_push_protection"]["status"],
        "disabled"
    );
    call(
        router,
        "POST",
        &format!("{item}/attach"),
        writer,
        json!({"scope":"selected","selected_repository_ids":[-1]}),
        "code-security/attach-configuration",
        422,
    )
    .await;
    call(
        router,
        "POST",
        &format!("{item}/attach"),
        outsider,
        json!({"scope":"all"}),
        "code-security/attach-configuration",
        403,
    )
    .await;
    call(
        router,
        "DELETE",
        &format!("{base}/detach"),
        writer,
        json!({"selected_repository_ids":[repo]}),
        "code-security/detach-configuration",
        204,
    )
    .await;
    call(
        router,
        "GET",
        repo_config,
        reader,
        Value::Null,
        "code-security/get-configuration-for-repository",
        204,
    )
    .await;
    let settings = codeyard_db_security::code_security::repository_settings(pool, repo)
        .await
        .unwrap()
        .unwrap();
    assert!(settings.configuration_id.is_none());
    assert_eq!(
        serde_json::from_str::<Value>(&settings.security_and_analysis).unwrap()["secret_scanning"]
            ["status"],
        "enabled"
    );
    transfers::contract(
        router,
        pool,
        new_repo["id"].as_i64().unwrap(),
        id,
        writer,
        reader,
    )
    .await;
    call(
        router,
        "DELETE",
        &item,
        reader,
        Value::Null,
        "code-security/delete-configuration",
        403,
    )
    .await;
    call(
        router,
        "DELETE",
        &item,
        writer,
        Value::Null,
        "code-security/delete-configuration",
        204,
    )
    .await;
    call(
        router,
        "GET",
        &item,
        reader,
        Value::Null,
        "code-security/get-configuration",
        404,
    )
    .await;
    call(
        router,
        "GET",
        "/api/v3/repos/acme/default-security/code-security-configuration",
        reader,
        Value::Null,
        "code-security/get-configuration-for-repository",
        204,
    )
    .await;
}

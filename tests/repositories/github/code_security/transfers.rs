use super::call;
use super::json;
use super::Router;
use super::Value;

pub(super) async fn contract(
    router: &Router,
    pool: &codeyard_db_core::DbPool,
    repo: i64,
    config: i64,
    writer: &str,
    reader: &str,
) {
    let transferred = call(
        router,
        "POST",
        "/api/v3/repos/acme/default-security/transfer",
        writer,
        json!({"new_owner":"alice"}),
        "repos/transfer",
        202,
    )
    .await;
    assert_eq!(transferred["owner"]["login"], "alice");
    assert_eq!(
        transferred["security_and_analysis"]["secret_scanning"]["status"],
        "enabled"
    );
    call(
        router,
        "GET",
        "/api/v3/repos/alice/default-security/code-security-configuration",
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
    assert!(
        settings.configuration_id.is_none(),
        "ownership changes must remove the former organization's association"
    );
    call(
        router,
        "PATCH",
        &format!("/api/v3/orgs/acme/code-security/configurations/{config}"),
        writer,
        json!({"secret_scanning":"disabled"}),
        "code-security/update-configuration",
        200,
    )
    .await;
    let personal = call(
        router,
        "GET",
        "/api/v3/repos/alice/default-security",
        reader,
        Value::Null,
        "repos/get",
        200,
    )
    .await;
    assert_eq!(
        personal["security_and_analysis"]["secret_scanning"]["status"], "enabled",
        "the former owner's policy no longer controls a transferred repository"
    );

    let transferred = call(
        router,
        "POST",
        "/api/v3/repos/alice/default-security/transfer",
        writer,
        json!({"new_owner":"acme"}),
        "repos/transfer",
        202,
    )
    .await;
    assert_eq!(transferred["owner"]["login"], "acme");
    assert_eq!(
        transferred["security_and_analysis"]["secret_scanning"]["status"],
        "disabled"
    );
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
    assert_eq!(
        linked["configuration"]["id"], config,
        "the destination organization's default is applied within the transfer transaction"
    );
}

use super::{request, roles::repo};
use crate::support::{
    codeyard_db_orgs::organizations::applied, database::Database, http_client::Client,
};

use codeyard_db_orgs::organizations::NewOrganization;
use codeyard_db_orgs::organizations::OrganizationProfile;

use codeyard_db_core::models::OrgRole;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let mut orgs = Vec::new();
    for name in ["acme", "other"] {
        orgs.push(applied(
            codeyard_db_orgs::organizations::create_organization(
                f.pool(),
                owner,
                &NewOrganization::new(name, OrganizationProfile::new(None, None).unwrap()).unwrap(),
            )
            .await
            .unwrap(),
        ));
    }
    applied(
        codeyard_db_orgs::organizations::add_organization_member(
            f.pool(),
            owner,
            orgs[0].id,
            member,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let first = repo(&f, root.path(), owner, orgs[0].id, "first").await;
    let second = repo(&f, root.path(), owner, orgs[0].id, "second").await;
    let foreign = repo(&f, root.path(), owner, orgs[1].id, "foreign").await;
    let router = f.router(root.path()).await;
    let mut credentials = Vec::new();
    for (actor, scope) in [(owner, "write"), (owner, "read"), (member, "write")] {
        let session = Client::login(&router, f.pool(), actor).await;
        let token = codeyard_identity::tokens::create(
            f.pool(),
            actor,
            Some(&session.session),
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
    let write = Some(credentials[0].as_str());
    let read = Some(credentials[1].as_str());
    let member = Some(credentials[2].as_str());
    let base = "/api/v3/orgs/acme/settings/immutable-releases";
    let selected = format!("{base}/repositories");
    let (_, initial) = request(
        &router,
        "orgs/get-immutable-releases-settings",
        "GET",
        base,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(initial["enforced_repositories"], "none");
    for token in [read, member] {
        request(
            &router,
            "orgs/set-immutable-releases-settings",
            "PUT",
            base,
            token,
            json!({"enforced_repositories":"all"}),
            403,
        )
        .await;
    }
    request(
        &router,
        "orgs/get-immutable-releases-settings",
        "GET",
        base,
        member,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "orgs/set-immutable-releases-settings",
        "PUT",
        base,
        write,
        json!({"enforced_repositories":"selected","selected_repository_ids":[first.id]}),
        204,
    )
    .await;
    request(
        &router,
        "orgs/enable-selected-repository-immutable-releases-organization",
        "PUT",
        &format!("{selected}/{}", second.id),
        write,
        Value::Null,
        204,
    )
    .await;
    let (headers, list) = request(
        &router,
        "orgs/get-immutable-releases-settings-repositories",
        "GET",
        &format!("{selected}?per_page=1"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list["total_count"], 2);
    assert_eq!(list["repositories"][0]["id"], first.id);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    request(
        &router,
        "orgs/disable-selected-repository-immutable-releases-organization",
        "DELETE",
        &format!("{selected}/{}", first.id),
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/set-immutable-releases-settings-repositories",
        "PUT",
        &selected,
        write,
        json!({"selected_repository_ids":[first.id,foreign.id]}),
        422,
    )
    .await;
    let (_, list) = request(
        &router,
        "orgs/get-immutable-releases-settings-repositories",
        "GET",
        &selected,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list["repositories"][0]["id"], second.id);
    request(
        &router,
        "orgs/set-immutable-releases-settings-repositories",
        "PUT",
        &selected,
        write,
        json!({"selected_repository_ids":[first.id,first.id]}),
        204,
    )
    .await;
    for mode in ["all", "none", "selected"] {
        request(
            &router,
            "orgs/set-immutable-releases-settings",
            "PUT",
            base,
            write,
            json!({"enforced_repositories":mode}),
            204,
        )
        .await;
        let (_, value) = request(
            &router,
            "orgs/get-immutable-releases-settings",
            "GET",
            base,
            read,
            Value::Null,
            200,
        )
        .await;
        assert_eq!(value["enforced_repositories"], mode);
    }
    let base = "/api/v3/orgs/acme/properties/installations";
    request(
        &router,
        "orgs/external-properties-for-repos-register-organization-app-installation",
        "POST",
        base,
        member,
        json!({"display_name":"Scanner","installation_id":42}),
        403,
    )
    .await;
    request(
        &router,
        "orgs/external-properties-for-repos-register-organization-app-installation",
        "POST",
        base,
        write,
        json!({"display_name":"Scanner","installation_id":42}),
        422,
    )
    .await;
    let (_, list) = request(
        &router,
        "orgs/external-properties-for-repos-get-organization-app-installations",
        "GET",
        base,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list, json!([]));
    let browser = Client::login(&router, f.pool(), owner).await;
    let response = browser
        .form_reply(
            "/settings/apps/new",
            &[
                ("name", "Scanner"),
                ("slug", "scanner"),
                ("homepage", "https://example.test/scanner"),
                (
                    "permissions",
                    r#"{"organization_external_properties_for_repos":"write"}"#,
                ),
                ("events", ""),
            ],
        )
        .await;
    assert_eq!(response.status, 200, "{}", response.text());
    let app = codeyard_db_identity::github_apps::app(f.pool(), "scanner")
        .await
        .unwrap()
        .unwrap();
    let installation = codeyard_db_identity::github_apps::install(
        f.pool(),
        owner,
        &browser.session,
        app.id,
        None,
        Some(orgs[0].id),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    request(
        &router,
        "orgs/external-properties-for-repos-register-organization-app-installation",
        "POST",
        base,
        write,
        json!({"display_name":"Scanner","installation_id":installation.id}),
        201,
    )
    .await;
    request(
        &router,
        "orgs/external-properties-for-repos-get-organization-definitions",
        "GET",
        &format!("{base}/schema"),
        member,
        Value::Null,
        403,
    )
    .await;
    let values = format!("{base}/values");
    request(&router, "orgs/external-properties-for-repos-create-or-update-organization-values", "PATCH", &values, write, json!({"repository_names":["first","second"],"properties":[{"property_name":"Scanner.risk","value":["low","reviewed"]}]}), 204).await;
    let (_, definitions) = request(
        &router,
        "orgs/external-properties-for-repos-get-organization-definitions",
        "GET",
        &format!("{base}/schema"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(definitions, json!([{"property_name":"Scanner.risk"}]));
    let property = format!("{values}/Scanner.risk");
    request(&router, "orgs/external-properties-for-repos-create-or-update-values-for-organization-property", "PATCH", &property, write, json!({"repository_values":[{"repository_name":"first","value":"high"},{"repository_name":"second","value":null}]}), 204).await;
    assert_eq!(f.scalar("SELECT COUNT(*) FROM organization_external_property_values WHERE org_id=$1 AND value_json='\"high\"'", &[orgs[0].id]).await, 1);
    request(&router, "orgs/external-properties-for-repos-create-or-update-values-for-organization-property", "PATCH", &property, write, json!({"repository_values":[{"repository_name":"first","value":"bad"},{"repository_name":"foreign","value":"bad"}]}), 422).await;
    assert_eq!(f.scalar("SELECT COUNT(*) FROM organization_external_property_values WHERE org_id=$1 AND value_json='\"high\"'", &[orgs[0].id]).await, 1);
    let (_, other) = request(
        &router,
        "orgs/external-properties-for-repos-get-organization-definitions",
        "GET",
        "/api/v3/orgs/other/properties/installations/schema",
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(other, json!([]));
    request(
        &router,
        "orgs/external-properties-for-repos-delete-organization-values",
        "DELETE",
        &property,
        member,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "orgs/external-properties-for-repos-delete-organization-values",
        "DELETE",
        &property,
        write,
        Value::Null,
        204,
    )
    .await;
    let (_, definitions) = request(
        &router,
        "orgs/external-properties-for-repos-get-organization-definitions",
        "GET",
        &format!("{base}/schema"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(definitions, json!([]));
    enterprise(&router).await;
    f.finish().await;
}

async fn enterprise(router: &axum::Router) {
    for (operation, method, path) in [
        (
            "code-security/get-configurations-for-enterprise",
            "GET",
            "code-security/configurations",
        ),
        (
            "code-security/create-configuration-for-enterprise",
            "POST",
            "code-security/configurations",
        ),
        (
            "code-security/get-default-configurations-for-enterprise",
            "GET",
            "code-security/configurations/defaults",
        ),
        (
            "code-security/get-single-configuration-for-enterprise",
            "GET",
            "code-security/configurations/1",
        ),
        (
            "code-security/update-enterprise-configuration",
            "PATCH",
            "code-security/configurations/1",
        ),
        (
            "code-security/delete-configuration-for-enterprise",
            "DELETE",
            "code-security/configurations/1",
        ),
        (
            "code-security/attach-enterprise-configuration",
            "POST",
            "code-security/configurations/1/attach",
        ),
        (
            "code-security/set-configuration-as-default-for-enterprise",
            "PUT",
            "code-security/configurations/1/defaults",
        ),
        (
            "code-security/get-repositories-for-enterprise-configuration",
            "GET",
            "code-security/configurations/1/repositories",
        ),
        (
            "dependabot/list-alerts-for-enterprise",
            "GET",
            "dependabot/alerts",
        ),
        (
            "dependabot/repository-access-for-enterprise",
            "GET",
            "dependabot/repository-access",
        ),
        (
            "dependabot/update-repository-access-for-enterprise",
            "PATCH",
            "dependabot/repository-access",
        ),
        (
            "dependabot/set-repository-access-default-level-for-enterprise",
            "PUT",
            "dependabot/repository-access/default-level",
        ),
        (
            "oidc/list-oidc-custom-property-inclusions-for-enterprise",
            "GET",
            "actions/oidc/customization/properties/repo",
        ),
        (
            "oidc/create-oidc-custom-property-inclusion-for-enterprise",
            "POST",
            "actions/oidc/customization/properties/repo",
        ),
        (
            "oidc/delete-oidc-custom-property-inclusion-for-enterprise",
            "DELETE",
            "actions/oidc/customization/properties/repo/risk",
        ),
    ] {
        let (_, body) = request(
            router,
            operation,
            method,
            &format!("/api/v3/enterprises/acme/{path}"),
            None,
            json!({}),
            404,
        )
        .await;
        assert_eq!(body["message"], "Not Found");
        assert_eq!(body["status"], "404");
    }
}

#[tokio::test]
async fn organization_release_settings_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn organization_release_settings_postgres() {
    contract(true).await;
}

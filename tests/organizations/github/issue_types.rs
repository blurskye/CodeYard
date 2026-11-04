use super::request;
use crate::support::{database::Database, http_client::Client, organizations::applied};
use codeyard::backend::db;
use codeyard::backend::db::organizations;
use codeyard::backend::db::organizations::NewOrganization;
use codeyard::backend::db::organizations::OrganizationProfile;
use codeyard::backend::db::repositories::creation::NewRepository;
use codeyard::backend::db::repositories::creation::RepositoryCreationOwner;
use codeyard::backend::db::repositories::creation::RepositoryTarget;
use codeyard::backend::identity::tokens;
use codeyard::backend::identity::tokens::TokenInput;
use codeyard_db_core::models::OrgRole;
use serde_json::{json, Value};

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let outsider = f.actor("outsider").await;
    let org = applied(
        organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        organizations::add_organization_member(
            f.pool(),
            owner,
            org.id,
            member,
            codeyard_db_core::models::OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("other", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let mut credentials = Vec::new();
    for (actor, scope) in [
        (owner, "write"),
        (owner, "read"),
        (member, "write"),
        (outsider, "write"),
    ] {
        let session = Client::login(&router, f.pool(), actor).await;
        let token = tokens::create(
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
    let outsider = Some(credentials[3].as_str());
    let path = "/api/v3/orgs/acme/issue-types";
    let input = json!({"name":"Bug", "is_enabled":true, "description":"A defect", "color":"red"});
    request(
        &router,
        "orgs/create-issue-type",
        "POST",
        path,
        read,
        input.clone(),
        403,
    )
    .await;
    request(
        &router,
        "orgs/create-issue-type",
        "POST",
        path,
        member,
        input.clone(),
        403,
    )
    .await;
    request(
        &router,
        "orgs/create-issue-type",
        "POST",
        path,
        outsider,
        input.clone(),
        403,
    )
    .await;
    request(
        &router,
        "orgs/create-issue-type",
        "POST",
        path,
        write,
        json!({"name":"Bug"}),
        422,
    )
    .await;
    request(
        &router,
        "orgs/create-issue-type",
        "POST",
        path,
        write,
        json!({"name":" ", "is_enabled":true}),
        422,
    )
    .await;
    request(
        &router,
        "orgs/create-issue-type",
        "POST",
        path,
        write,
        json!({"name":"Bug", "is_enabled":true,"color":"invalid"}),
        422,
    )
    .await;
    // Definitions do not need a repository to exist first.
    let (_, body) = request(
        &router,
        "orgs/create-issue-type",
        "POST",
        path,
        write,
        input.clone(),
        200,
    )
    .await;
    let id = body["id"].as_i64().unwrap();
    assert_eq!(body["name"], "Bug");
    assert_eq!(body["description"], "A defect");
    assert_eq!(body["color"], "red");
    request(
        &router,
        "orgs/create-issue-type",
        "POST",
        path,
        write,
        json!({"name":"bug","is_enabled":true}),
        422,
    )
    .await;
    for token in [read, member] {
        let (_, body) = request(
            &router,
            "orgs/list-issue-types",
            "GET",
            path,
            token,
            Value::Null,
            200,
        )
        .await;
        assert_eq!(body[0]["id"], id);
    }
    request(
        &router,
        "orgs/list-issue-types",
        "GET",
        path,
        None,
        Value::Null,
        401,
    )
    .await;
    request(
        &router,
        "orgs/list-issue-types",
        "GET",
        path,
        outsider,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "orgs/update-issue-type",
        "PUT",
        &format!("/api/v3/orgs/other/issue-types/{id}"),
        write,
        input,
        404,
    )
    .await;
    request(
        &router,
        "orgs/delete-issue-type",
        "DELETE",
        &format!("/api/v3/orgs/other/issue-types/{id}"),
        write,
        Value::Null,
        404,
    )
    .await;
    let disk = uuid::Uuid::new_v4().to_string();
    db::repositories::creation::insert_repository(
        f.pool(),
        NewRepository {
            owner: RepositoryCreationOwner {
                actor_id: owner,
                target: RepositoryTarget::Organization(org.id),
            },
            name: "tools",
            description: None,
            is_private: false,
            disk_path: &disk,
            storage_node_id: None,
            source_id: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let (_, body) = request(
        &router,
        "repos/list-issue-types",
        "GET",
        "/api/v3/repos/acme/tools/issue-types",
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["id"], id);
    let (_, body) = request(
        &router,
        "orgs/update-issue-type",
        "PUT",
        &format!("{path}/{id}"),
        write,
        json!({"name":"Defect", "is_enabled":false, "description":null, "color":null}),
        200,
    )
    .await;
    assert_eq!(body["id"], id);
    assert_eq!(body["name"], "Defect");
    assert!(body["color"].is_null());
    let (_, body) = request(
        &router,
        "orgs/list-issue-types",
        "GET",
        path,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body[0]["is_enabled"], false);
    let (_, body) = request(
        &router,
        "repos/list-issue-types",
        "GET",
        "/api/v3/repos/acme/tools/issue-types",
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body, json!([]));
    request(
        &router,
        "orgs/delete-issue-type",
        "DELETE",
        &format!("{path}/{id}"),
        read,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "orgs/delete-issue-type",
        "DELETE",
        &format!("{path}/{id}"),
        member,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        "orgs/delete-issue-type",
        "DELETE",
        &format!("{path}/{id}"),
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/delete-issue-type",
        "DELETE",
        &format!("{path}/{id}"),
        write,
        Value::Null,
        404,
    )
    .await;
    let (_, body) = request(
        &router,
        "orgs/list-issue-types",
        "GET",
        path,
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body, json!([]));
    f.finish().await;
}

#[tokio::test]
async fn sqlite_organization_issue_types() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_organization_issue_types() {
    contract(true).await;
}

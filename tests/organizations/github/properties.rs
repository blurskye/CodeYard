use super::request;
use crate::support::{database::Database, http_client::Client, organizations::applied};
use codeyard::backend::db;
use codeyard::backend::db::organizations;
use codeyard::backend::db::organizations::properties;
use codeyard::backend::db::organizations::properties::PropertyValue;
use codeyard::backend::db::organizations::properties::ValueInput;
use codeyard::backend::db::organizations::NewOrganization;
use codeyard::backend::db::organizations::OrganizationProfile;
use codeyard::backend::db::repositories::creation::NewRepository;
use codeyard::backend::db::repositories::creation::RepositoryCreationOwner;
use codeyard::backend::db::repositories::creation::RepositoryTarget;
use codeyard::backend::identity::tokens;
use codeyard::backend::identity::tokens::TokenInput;
use codeyard_db_core::models::OrgRole;
use serde_json::{json, Value};
mod lifecycle;
mod native;

const LIST: &str = "orgs/custom-properties-for-repos-get-organization-definitions";
const MANY: &str = "orgs/custom-properties-for-repos-create-or-update-organization-definitions";
const GET: &str = "orgs/custom-properties-for-repos-get-organization-definition";
const PUT: &str = "orgs/custom-properties-for-repos-create-or-update-organization-definition";
const DELETE: &str = "orgs/custom-properties-for-repos-delete-organization-definition";
const VALUES: &str = "orgs/custom-properties-for-repos-get-organization-values";
const SET: &str = "orgs/custom-properties-for-repos-create-or-update-organization-values";
async fn repository(
    f: &Database,
    root: &std::path::Path,
    owner: i64,
    org: i64,
    name: &str,
) -> codeyard_db_core::models::Repository {
    let disk = uuid::Uuid::new_v4().to_string();
    let git = git2::Repository::init_bare(root.join(&disk)).unwrap();
    git.set_head("refs/heads/main").unwrap();
    db::repositories::creation::insert_repository(
        f.pool(),
        NewRepository {
            owner: RepositoryCreationOwner {
                actor_id: owner,
                target: RepositoryTarget::Organization(org),
            },
            name,
            description: None,
            is_private: true,
            disk_path: &disk,
            storage_node_id: None,
            source_id: None,
        },
    )
    .await
    .unwrap()
    .unwrap()
}
async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let outside = f.actor("outside").await;
    let delegated = f.actor("delegated").await;
    let org = applied(
        organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let other = applied(
        organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("foreign", OrganizationProfile::new(None, None).unwrap())
                .unwrap(),
        )
        .await
        .unwrap(),
    );
    for (user, role) in [
        (member, codeyard_db_core::models::OrgRole::Member),
        (delegated, codeyard_db_core::models::OrgRole::Admin),
    ] {
        applied(
            organizations::add_organization_member(f.pool(), owner, org.id, user, role)
                .await
                .unwrap(),
        );
    }
    let root = tempfile::tempdir().unwrap();
    let first = repository(&f, root.path(), owner, org.id, "first").await;
    let second = repository(&f, root.path(), owner, org.id, "second").await;
    let foreign = repository(&f, root.path(), owner, other.id, "secret").await;
    let router = f.router(root.path()).await;
    let mut credentials = Vec::new();
    for (user, scope) in [
        (owner, "write"),
        (owner, "read"),
        (member, "write"),
        (outside, "write"),
        (delegated, "write"),
    ] {
        let browser = Client::login(&router, f.pool(), user).await;
        let token = tokens::create(
            f.pool(),
            user,
            Some(&browser.session),
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
    let member_auth = Some(credentials[2].as_str());
    let outside_auth = Some(credentials[3].as_str());
    let delegated_auth = Some(credentials[4].as_str());
    let schema = "/api/v3/orgs/acme/properties/schema";
    let values = "/api/v3/orgs/acme/properties/values";
    for (auth, status) in [(None, 401), (outside_auth, 403)] {
        request(&router, LIST, "GET", schema, auth, Value::Null, status).await;
    }
    for auth in [read, member_auth, outside_auth, delegated_auth] {
        request(
            &router,
            PUT,
            "PUT",
            &format!("{schema}/env"),
            auth,
            json!({"value_type":"string"}),
            403,
        )
        .await;
    }
    request(
        &router,
        GET,
        "GET",
        &format!("{schema}/missing"),
        write,
        Value::Null,
        404,
    )
    .await;
    let (_,env)=request(&router,PUT,"PUT",&format!("{schema}/env"),write,json!({"value_type":"single_select","allowed_values":["dev","prod"],"default_value":"dev","description":"Deployment environment","values_editable_by":"org_and_repo_actors"}),200).await;
    assert_eq!(env["source_type"], "organization");
    assert!(env["url"]
        .as_str()
        .unwrap()
        .ends_with("/api/v3/orgs/acme/properties/schema/env"));
    request(
        &router,
        GET,
        "GET",
        &format!("{schema}/ENV"),
        member_auth,
        Value::Null,
        200,
    )
    .await;
    request(&router,MANY,"PATCH",schema,write,json!({"properties":[
        {"property_name":"tags","value_type":"multi_select","allowed_values":["red","blue"],"default_value":["red"],"values_editable_by":"org_and_repo_actors"},
        {"property_name":"enabled","value_type":"true_false","required":true,"default_value":"false"},
        {"property_name":"docs","value_type":"url","default_value":"https://example.org/docs"},
        {"property_name":"label","value_type":"string"}
    ]}),200).await;
    let (_, catalog) = request(&router, LIST, "GET", schema, member_auth, Value::Null, 200).await;
    assert_eq!(catalog.as_array().unwrap().len(), 5);
    let (_, hidden) = request(
        &router,
        VALUES,
        "GET",
        values,
        member_auth,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(hidden, json!([]));
    let (headers, page) = request(
        &router,
        VALUES,
        "GET",
        &format!("{values}?per_page=1"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(page[0]["repository_id"], first.id);
    assert_eq!(page[0]["repository_full_name"], "acme/first");
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let properties = page[0]["properties"].as_array().unwrap();
    assert_eq!(
        properties
            .iter()
            .find(|property| property["property_name"] == "enabled")
            .unwrap()["value"],
        "false"
    );
    request(&router,SET,"PATCH",values,write,json!({"repository_names":["first","second"],"properties":[{"property_name":"env","value":"prod"},{"property_name":"label","value":"Visible metadata"}]}),204).await;
    request(
        &router,
        SET,
        "PATCH",
        values,
        write,
        json!({"repository_names":["second"],"properties":[{"property_name":"env","value":"dev"}]}),
        204,
    )
    .await;
    for query in [
        "props.env:prod",
        "repo:acme/first",
        "name:first AND archived:false",
        "props.env:prod OR repo:foreign/secret",
    ] {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("repository_query", query)
            .finish();
        let (_, selected) = request(
            &router,
            VALUES,
            "GET",
            &format!("{values}?{query}"),
            write,
            Value::Null,
            200,
        )
        .await;
        assert_eq!(selected.as_array().unwrap().len(), 1);
        assert_eq!(selected[0]["repository_id"], first.id);
    }
    let (_, selected) = request(
        &router,
        VALUES,
        "GET",
        &format!("{values}?repository_query=-props.env:prod"),
        write,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(selected[0]["repository_id"], second.id);
    request(
        &router,
        VALUES,
        "GET",
        &format!("{values}?repository_query=props.unknown:value"),
        write,
        Value::Null,
        422,
    )
    .await;
    request(
        &router,
        VALUES,
        "GET",
        &format!("{values}?repository_query=archived:maybe"),
        write,
        Value::Null,
        422,
    )
    .await;
    request(&router,SET,"PATCH",values,write,json!({"repository_names":["first","foreign/secret"],"properties":[{"property_name":"env","value":"dev"}]}),422).await;
    let (_, selected) = request(
        &router,
        VALUES,
        "GET",
        &format!("{values}?repository_query=props.env:prod"),
        write,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(selected[0]["repository_id"], first.id);
    for payload in [
        json!({"repository_names":["first"],"properties":[{"property_name":"env"}]}),
        json!({"repository_names":["first"],"properties":[{"property_name":"env","value":"unknown"}]}),
        json!({"repository_names":["first"],"properties":[{"property_name":"enabled","value":true}]}),
        json!({"repository_names":["first"],"properties":[{"property_name":"docs","value":"javascript:alert(1)"}]}),
        json!({"repository_names":["first"],"properties":[{"property_name":"tags","value":["red","red"]}]}),
        json!({"repository_names":["first"],"properties":[{"property_name":"unknown","value":"x"}]}),
    ] {
        request(&router, SET, "PATCH", values, write, payload, 422).await;
    }
    for auth in [read, member_auth, outside_auth, delegated_auth] {
        request(&router,SET,"PATCH",values,auth,json!({"repository_names":["first"],"properties":[{"property_name":"label","value":"denied"}]}),403).await;
    }
    // An outside repository administrator can edit only properties explicitly delegated to repositories.
    f.execute(
        "INSERT INTO repo_access(user_id,repo_id,access_level) VALUES($1,$2,5)",
        &[outside, first.id],
    )
    .await;
    properties::set_repository_values(
        f.pool(),
        outside,
        first.id,
        &[ValueInput {
            property_name: "env".into(),
            value: Some(PropertyValue::String("dev".into())),
        }],
    )
    .await
    .unwrap();
    assert!(properties::set_repository_values(
        f.pool(),
        outside,
        first.id,
        &[ValueInput {
            property_name: "label".into(),
            value: Some(PropertyValue::String("denied".into()))
        }]
    )
    .await
    .is_err());
    assert!(properties::repository(f.pool(), Some(outside), second.id)
        .await
        .unwrap()
        .is_none());
    let visible = properties::repository(f.pool(), Some(outside), first.id)
        .await
        .unwrap()
        .unwrap();
    assert!(visible.repository_admin);
    assert!(!visible.organization_owner);
    // PUT and batch PATCH replace optional definition fields rather than merging old values.
    let (_,reset)=request(&router,PUT,"PUT",&format!("{schema}/env"),write,json!({"value_type":"single_select","allowed_values":["dev","prod"],"values_editable_by":null}),200).await;
    assert_eq!(reset["values_editable_by"], "org_actors");
    assert!(reset["default_value"].is_null());
    assert!(reset["description"].is_null());
    assert!(properties::set_repository_values(
        f.pool(),
        outside,
        first.id,
        &[ValueInput {
            property_name: "env".into(),
            value: Some(PropertyValue::String("prod".into()))
        }]
    )
    .await
    .is_err());
    // Incompatible select changes and invalid bulk definitions preserve every previous definition/value.
    request(&router,MANY,"PATCH",schema,write,json!({"properties":[{"property_name":"temporary","value_type":"string"},{"property_name":"env","value_type":"single_select","allowed_values":["only"]}]}),422).await;
    request(
        &router,
        GET,
        "GET",
        &format!("{schema}/temporary"),
        write,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        PUT,
        "PUT",
        &format!("{schema}/bad"),
        write,
        json!({"value_type":"true_false","default_value":false}),
        422,
    )
    .await;
    request(
        &router,
        PUT,
        "PUT",
        &format!("{schema}/bad"),
        write,
        json!({"value_type":"string","source_type":"enterprise"}),
        422,
    )
    .await;
    let denied = db::repositories::creation::insert_repository_with_properties(
        f.pool(),
        NewRepository {
            owner: RepositoryCreationOwner {
                actor_id: delegated,
                target: RepositoryTarget::Organization(org.id),
            },
            name: "blocked",
            description: None,
            is_private: true,
            disk_path: "blocked.git",
            storage_node_id: None,
            source_id: None,
        },
        &[ValueInput {
            property_name: "label".into(),
            value: Some(PropertyValue::String("denied".into())),
        }],
    )
    .await
    .unwrap_err();
    assert!(matches!(
        denied.downcast_ref::<properties::Refusal>(),
        Some(properties::Refusal::Denied)
    ));
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repositories WHERE owner_org_id=$1 AND name='blocked'",
            &[org.id]
        )
        .await,
        0
    );
    // Required explicit metadata is validated in the repository-creation authority transaction.
    request(
        &router,
        PUT,
        "PUT",
        &format!("{schema}/ticket"),
        write,
        json!({"value_type":"string"}),
        200,
    )
    .await;
    request(&router,SET,"PATCH",values,write,json!({"repository_names":["first","second"],"properties":[{"property_name":"ticket","value":"ready"}]}),204).await;
    request(
        &router,
        PUT,
        "PUT",
        &format!("{schema}/ticket"),
        write,
        json!({"value_type":"string","required":true,"require_explicit_values":true}),
        200,
    )
    .await;
    request(&router,SET,"PATCH",values,write,json!({"repository_names":["first"],"properties":[{"property_name":"ticket","value":null}]}),422).await;
    let input = || NewRepository {
        owner: RepositoryCreationOwner {
            actor_id: owner,
            target: RepositoryTarget::Organization(org.id),
        },
        name: "future",
        description: None,
        is_private: true,
        disk_path: "future.git",
        storage_node_id: None,
        source_id: None,
    };
    assert!(
        db::repositories::creation::insert_repository(f.pool(), input())
            .await
            .is_err()
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repositories WHERE owner_org_id=$1 AND name='future'",
            &[org.id]
        )
        .await,
        0
    );
    let future = db::repositories::creation::insert_repository_with_properties(
        f.pool(),
        input(),
        &[ValueInput {
            property_name: "ticket".into(),
            value: Some(PropertyValue::String("new".into())),
        }],
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        properties::repository(f.pool(), Some(owner), future.id)
            .await
            .unwrap()
            .unwrap()
            .values
            .properties
            .iter()
            .find(|value| value.property_name == "ticket")
            .unwrap()
            .value,
        Some(PropertyValue::String("new".into()))
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repository_property_values WHERE repo_id=$1",
            &[foreign.id]
        )
        .await,
        0
    );
    let ticket=f.scalar("SELECT id FROM organization_repository_properties WHERE org_id=$1 AND property_name='ticket'",&[org.id]).await;
    request(
        &router,
        DELETE,
        "DELETE",
        &format!("{schema}/ticket"),
        member_auth,
        Value::Null,
        403,
    )
    .await;
    request(
        &router,
        DELETE,
        "DELETE",
        &format!("{schema}/ticket"),
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        DELETE,
        "DELETE",
        &format!("{schema}/ticket"),
        write,
        Value::Null,
        404,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repository_property_values WHERE definition_id=$1",
            &[ticket]
        )
        .await,
        0
    );
    native::contract(&f, &router, [owner, member, outside], (org.id, first.id)).await;
    lifecycle::contract(&f, owner, org.id, other.id, [first.id, second.id]).await;
    f.finish().await;
}
#[tokio::test]
async fn github_organization_properties_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn github_organization_properties_postgres() {
    contract(true).await;
}

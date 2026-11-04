use super::request;
use crate::support::{
    codeyard::backend::db::organizations::applied,
    database::Database,
    http_client::{field_value, hidden_value, Client},
};
use axum::http::StatusCode;

use codeyard::backend::db::organizations::NewOrganization;
use codeyard::backend::db::organizations::OrganizationProfile;
use codeyard::backend::db::repositories::creation::NewRepository;
use codeyard::backend::db::repositories::creation::RepositoryCreationOwner;
use codeyard::backend::db::repositories::creation::RepositoryTarget;

use codeyard::backend::identity::tokens::TokenInput;
use codeyard_db_core::models::OrgRole;
use serde_json::{json, Value};

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let delegated = f.actor("delegated").await;
    let outsider = f.actor("outside").await;
    let mut orgs = Vec::new();
    for name in ["acme", "other"] {
        orgs.push(applied(
            codeyard::backend::db::organizations::create_organization(
                f.pool(),
                owner,
                &NewOrganization::new(name, OrganizationProfile::new(None, None).unwrap()).unwrap(),
            )
            .await
            .unwrap(),
        ));
    }
    for (user, role) in [(member, OrgRole::Member), (delegated, OrgRole::Admin)] {
        applied(
            codeyard::backend::db::organizations::add_organization_member(
                f.pool(),
                owner,
                orgs[0].id,
                user,
                role,
            )
            .await
            .unwrap(),
        );
    }
    let root = tempfile::tempdir().unwrap();
    let disk = uuid::Uuid::new_v4().to_string();
    git2::Repository::init_bare(root.path().join(&disk)).unwrap();
    codeyard::backend::db::repositories::creation::insert_repository(
        f.pool(),
        NewRepository {
            owner: RepositoryCreationOwner {
                actor_id: owner,
                target: RepositoryTarget::Organization(orgs[0].id),
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
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), owner).await;
    let mut credentials = Vec::new();
    for (user, scope) in [
        (owner, "write"),
        (owner, "read"),
        (member, "write"),
        (delegated, "write"),
        (outsider, "write"),
    ] {
        let session = Client::login(&router, f.pool(), user).await;
        let token = codeyard::backend::identity::tokens::create(
            f.pool(),
            user,
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
    let member_token = Some(credentials[2].as_str());
    let outside = Some(credentials[4].as_str());
    let path = "/api/v3/orgs/acme/issue-fields";
    request(
        &router,
        "orgs/list-issue-fields",
        "GET",
        path,
        None,
        Value::Null,
        401,
    )
    .await;
    request(
        &router,
        "orgs/list-issue-fields",
        "GET",
        path,
        outside,
        Value::Null,
        403,
    )
    .await;
    let input = json!({"name":"Priority","description":"How urgent","data_type":"single_select","options":[{"name":"High","color":"red","priority":10,"description":"Needs attention"},{"name":"Low","color":"green","priority":2,"description":null}]});
    for token in [read, member_token, Some(credentials[3].as_str()), outside] {
        request(
            &router,
            "orgs/create-issue-field",
            "POST",
            path,
            token,
            input.clone(),
            403,
        )
        .await;
    }
    for invalid in [
        json!({}),
        json!({"name":"Bad","data_type":"unsupported"}),
        json!({"name":"Bad","data_type":"text","visibility":null}),
        json!({"name":"Bad","data_type":"single_select"}),
        json!({"name":"Bad","data_type":"single_select","options":[{"name":"x","color":"red"}]}),
        json!({"name":"Bad","data_type":"text","options":[{"name":"x","color":"red","priority":0}]}),
        json!({"name":"Bad","data_type":"single_select","options":[{"name":"x","color":"invalid","priority":0}]}),
    ] {
        request(
            &router,
            "orgs/create-issue-field",
            "POST",
            path,
            write,
            invalid,
            422,
        )
        .await;
    }
    let (_, field) = request(
        &router,
        "orgs/create-issue-field",
        "POST",
        path,
        write,
        input,
        200,
    )
    .await;
    let id = field["id"].as_i64().unwrap();
    let url = format!("{path}/{id}");
    let low = field["options"][0]["id"].as_i64().unwrap();
    let high = field["options"][1]["id"].as_i64().unwrap();
    assert_eq!(field["visibility"], "organization_members_only");
    assert_eq!(field["options"][0]["name"], "Low");
    assert_eq!(field["options"][1]["description"], "Needs attention");
    let (_, listed) = request(
        &router,
        "orgs/list-issue-fields",
        "GET",
        path,
        member_token,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(listed, json!([field]));
    for token in [read, member_token, Some(credentials[3].as_str()), outside] {
        request(
            &router,
            "orgs/update-issue-field",
            "PATCH",
            &url,
            token,
            json!({"name":"No"}),
            403,
        )
        .await;
        request(
            &router,
            "orgs/delete-issue-field",
            "DELETE",
            &url,
            token,
            Value::Null,
            403,
        )
        .await;
    }
    let (_,foreign)=request(&router,"orgs/create-issue-field","POST","/api/v3/orgs/other/issue-fields",write,json!({"name":"Other","data_type":"single_select","options":[{"name":"foreign","color":"blue","priority":0}]}),200).await;
    request(
        &router,
        "orgs/update-issue-field",
        "PATCH",
        &format!("/api/v3/orgs/other/issue-fields/{id}"),
        write,
        json!({"name":"No"}),
        404,
    )
    .await;
    request(
        &router,
        "orgs/delete-issue-field",
        "DELETE",
        &format!("/api/v3/orgs/other/issue-fields/{id}"),
        write,
        Value::Null,
        404,
    )
    .await;
    request(&router,"orgs/update-issue-field","PATCH",&url,write,json!({"name":"Must roll back","options":[{"id":foreign["options"][0]["id"],"name":"Foreign","color":"red","priority":0}]}),422).await;
    for invalid in [
        json!({"name":null}),
        json!({"visibility":null}),
        json!({"options":null}),
        json!({"visibility":"secret"}),
    ] {
        request(
            &router,
            "orgs/update-issue-field",
            "PATCH",
            &url,
            write,
            invalid,
            422,
        )
        .await;
    }
    let (a, b) = tokio::join!(
        request(
            &router,
            "orgs/update-issue-field",
            "PATCH",
            &url,
            write,
            json!({"name":"Urgency"}),
            200
        ),
        request(
            &router,
            "orgs/update-issue-field",
            "PATCH",
            &url,
            write,
            json!({"description":"Concurrent description"}),
            200
        )
    );
    assert_eq!(a.1["name"], "Urgency");
    assert_eq!(b.1["description"], "Concurrent description");
    let (_, field) = request(
        &router,
        "orgs/update-issue-field",
        "PATCH",
        &url,
        write,
        json!({"visibility":"all"}),
        200,
    )
    .await;
    assert_eq!(field["name"], "Urgency");
    assert_eq!(field["description"], "Concurrent description");
    assert_eq!(field["options"][1]["id"], high);
    let (_, issue) = request(
        &router,
        "issues/create",
        "POST",
        "/api/v3/repos/acme/tools/issues",
        write,
        json!({"title":"Uses inherited organization fields"}),
        201,
    )
    .await;
    let values = format!(
        "/api/v3/repos/acme/tools/issues/{}/issue-field-values",
        issue["number"]
    );
    let (_, assigned) = request(
        &router,
        "issues/add-issue-field-values",
        "POST",
        &values,
        write,
        json!({"issue_field_values":[{"field_id":id,"value":"High"}]}),
        200,
    )
    .await;
    assert_eq!(assigned[0]["single_select_option"]["id"], high);
    request(&router,"orgs/update-issue-field","PATCH",&url,write,json!({"options":[{"id":high,"name":"Urgent","color":"purple","priority":10,"description":"Updated option"},{"id":low,"name":"Low","color":"green","priority":2}]}),200).await;
    let (_, assigned) = request(
        &router,
        "issues/list-issue-field-values-for-issue",
        "GET",
        &values,
        outside,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(assigned[0]["value"], "Urgent");
    assert_eq!(assigned[0]["single_select_option"]["id"], high);
    request(
        &router,
        "orgs/update-issue-field",
        "PATCH",
        &url,
        write,
        json!({"visibility":"organization_members_only"}),
        200,
    )
    .await;
    let (_, hidden) = request(
        &router,
        "issues/list-issue-field-values-for-issue",
        "GET",
        &values,
        outside,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(hidden, json!([]));
    request(
        &router,
        "orgs/update-issue-field",
        "PATCH",
        &url,
        write,
        json!({"options":[{"id":low,"name":"Low","color":"green","priority":2}]}),
        200,
    )
    .await;
    let (_, removed) = request(
        &router,
        "issues/list-issue-field-values-for-issue",
        "GET",
        &values,
        write,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(removed, json!([]));
    let issue_page = format!("/acme/tools/issues/{}", issue["number"]);
    let page = browser.page_ok(&issue_page).await;
    assert!(page.contains("New option description"));
    assert!(page.contains("New option priority"));
    let id_text = id.to_string();
    // The field's definition form carries the revision and owner the page was drawn from.
    let definition = page
        .split("<form")
        .find(|form| {
            field_value(form, "action").as_deref() == Some("configure")
                && field_value(form, "attribute_id").as_deref() == Some(id_text.as_str())
        })
        .expect("the issue page has the field's definition form");
    let expected_revision = hidden_value(definition, "expected_revision");
    let expected_updated = hidden_value(definition, "expected_updated");
    let owner_key = hidden_value(definition, "owner_key");
    let name_key = format!("option_{low}_name");
    let color_key = format!("option_{low}_color");
    let description_key = format!("option_{low}_description");
    let priority_key = format!("option_{low}_priority");
    let (status, page) = browser
        .form(
            &format!("{issue_page}/attributes"),
            &[
                ("action", "configure"),
                ("kind", "field"),
                ("attribute_id", &id_text),
                ("expected_revision", &expected_revision),
                ("expected_updated", &expected_updated),
                ("owner_key", &owner_key),
                ("name", "Urgency"),
                ("description", "Native field notes"),
                ("data_type", "single_select"),
                ("visibility", "all"),
                (&name_key, "Low"),
                (&color_key, "blue"),
                (&description_key, "Native option notes"),
                (&priority_key, "-4"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let (_, field) = request(
        &router,
        "orgs/update-issue-field",
        "PATCH",
        &url,
        write,
        json!({"description":null}),
        200,
    )
    .await;
    assert_eq!(field["description"], Value::Null);
    assert_eq!(field["options"][0]["description"], "Native option notes");
    assert_eq!(field["options"][0]["priority"], -4);
    for data_type in ["text", "number", "date", "multi_select"] {
        let input = if data_type == "multi_select" {
            json!({"name":data_type,"data_type":data_type,"options":[]})
        } else {
            json!({"name":data_type,"data_type":data_type})
        };
        let (_, created) = request(
            &router,
            "orgs/create-issue-field",
            "POST",
            path,
            write,
            input,
            200,
        )
        .await;
        assert_eq!(created["data_type"], data_type);
    }
    let (_, kind) = request(
        &router,
        "orgs/create-issue-type",
        "POST",
        "/api/v3/orgs/acme/issue-types",
        write,
        json!({"name":"Bug","is_enabled":true}),
        200,
    )
    .await;
    request(
        &router,
        "orgs/update-issue-field",
        "PATCH",
        &format!("{path}/{}", kind["id"]),
        write,
        json!({"name":"No"}),
        404,
    )
    .await;
    request(
        &router,
        "orgs/delete-issue-field",
        "DELETE",
        &format!("{path}/{}", kind["id"]),
        write,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        "orgs/delete-issue-field",
        "DELETE",
        &url,
        write,
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "orgs/delete-issue-field",
        "DELETE",
        &url,
        write,
        Value::Null,
        404,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn github_organization_issue_fields_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn github_organization_issue_fields_postgres() {
    contract(true).await;
}

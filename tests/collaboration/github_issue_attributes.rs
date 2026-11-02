use super::github::{document, request};
use crate::support::{database::Database, http_client::Client};
use axum::http::StatusCode;
use codeyard::backend::{
    db,
    identity::tokens::{self, TokenInput},
};
use serde_json::{json, Value};
async fn attributes(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let org = match db::organizations::create_organization(
        f.pool(),
        alice,
        &db::organizations::NewOrganization::new(
            "collective",
            db::organizations::OrganizationProfile::new(None, None).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    {
        db::organizations::OrganizationOutcome::Applied(org) => org,
        _ => panic!("create organization"),
    };
    use db::repositories::creation::{RepositoryCreationOwner, RepositoryTarget};
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner {
            actor_id: alice,
            target: RepositoryTarget::Organization(org.id),
        },
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let issue = db::issues::create_issue(
        f.pool(),
        repo.id,
        alice,
        "Attributes",
        "@bob mentioned, not @bobby",
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), alice).await;
    let outsider = Client::login(&router, f.pool(), bob).await;
    let mut credentials = Vec::new();
    for (actor, scope, session) in [
        (alice, "write", browser.session.as_str()),
        (alice, "read", browser.session.as_str()),
        (bob, "write", outsider.session.as_str()),
    ] {
        let token = tokens::create(
            f.pool(),
            actor,
            Some(session),
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
    let (write, read, other) = (&credentials[0], &credentials[1], &credentials[2]);
    let base = "/api/v3/repos/collective/proj";
    let path = format!("{base}/issues/{}", issue.number);
    let page = format!("/collective/proj/issues/{}", issue.number);
    let action = format!("{page}/attributes");
    let repository_id = repo.id.to_string();
    let item_id = issue.id.to_string();
    let owner_key = format!("org:{}", org.id);
    let created = browser
        .form_reply(
            &action,
            &[
                ("repository_id", &repository_id),
                ("item_id", &item_id),
                ("owner_key", &owner_key),
                ("action", "configure"),
                ("kind", "type"),
                ("name", "Bug"),
                ("is_enabled", "true"),
                ("color", "red"),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
    let types = document(
        request(
            &router,
            "GET",
            &format!("{base}/issue-types"),
            Some(read),
            Value::Null,
        )
        .await,
        "repos/list-issue-types",
        200,
    )
    .await;
    assert_eq!(types[0]["name"], "Bug");
    let bug = types[0]["id"].as_i64().unwrap();
    let typed = document(
        request(
            &router,
            "POST",
            &format!("{base}/issues"),
            Some(write),
            json!({"title":"Typed issue","type":"bug"}),
        )
        .await,
        "issues/create",
        201,
    )
    .await;
    assert_eq!(typed["type"]["id"], bug);
    let typed_path = format!("{base}/issues/{}", typed["number"]);
    let types = document(
        request(
            &router,
            "GET",
            &format!("{base}/issues?type=Bug"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-for-repo",
        200,
    )
    .await;
    assert_eq!(types.as_array().unwrap().len(), 1);
    assert_eq!(types[0]["id"], typed["id"]);
    // `gh issue list --type` searches with the issue type's name.
    let found = document(
        request(
            &router,
            "GET",
            "/api/v3/search/issues?q=repo%3Acollective%2Fproj+type%3Abug",
            Some(read),
            Value::Null,
        )
        .await,
        "search/issues-and-pull-requests",
        200,
    )
    .await;
    assert_eq!(found["total_count"], 1, "{found}");
    assert_eq!(found["items"][0]["id"], typed["id"]);
    let mentioned = document(
        request(
            &router,
            "GET",
            &format!("{base}/issues?mentioned=bob"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-for-repo",
        200,
    )
    .await;
    assert_eq!(mentioned.as_array().unwrap().len(), 1);
    assert_eq!(mentioned[0]["id"], issue.id);
    for data_type in ["text", "number", "date", "single_select", "multi_select"] {
        let mut form = vec![
            ("repository_id", repository_id.as_str()),
            ("item_id", item_id.as_str()),
            ("owner_key", owner_key.as_str()),
            ("action", "configure"),
            ("kind", "field"),
            ("name", data_type),
            ("data_type", data_type),
            ("visibility", "all"),
        ];
        if data_type.ends_with("select") {
            form.extend([("new_option_name", "High"), ("new_option_color", "red")]);
        }
        let response = browser.form_reply(&action, &form).await;
        assert_eq!(
            response.status,
            StatusCode::SEE_OTHER,
            "{}",
            response.text()
        );
    }
    let response = browser
        .form_reply(
            &action,
            &[
                ("repository_id", &repository_id),
                ("item_id", &item_id),
                ("owner_key", &owner_key),
                ("action", "configure"),
                ("kind", "field"),
                ("name", "Internal"),
                ("data_type", "text"),
                ("visibility", "organization_members_only"),
            ],
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "{}",
        response.text()
    );
    let fields = db::issue_attributes::catalog(f.pool(), repo.id, Some(alice), "field")
        .await
        .unwrap();
    let field = |name: &str| fields.iter().find(|f| f.name == name).unwrap();
    let values=fields.iter().map(|f|json!({"field_id":f.id,"value":match f.data_type.as_deref().unwrap(){"number"=>json!(42.5),"date"=>json!("2026-10-02"),"single_select"=>json!("High"),"multi_select"=>json!(["High"]),_ if f.name=="Internal"=>json!("private-field-secret"),_=>json!("visible <text>")}})).collect::<Vec<_>>();
    let endpoint = format!("{path}/issue-field-values");
    let values = document(
        request(
            &router,
            "POST",
            &endpoint,
            Some(write),
            json!({"issue_field_values":values}),
        )
        .await,
        "issues/add-issue-field-values",
        200,
    )
    .await;
    assert_eq!(values.as_array().unwrap().len(), 6);
    let text_node = values[0]["node_id"].clone();
    let listed = request(
        &router,
        "GET",
        &format!("{endpoint}?per_page=1"),
        Some(read),
        Value::Null,
    )
    .await;
    assert!(listed.headers()["link"]
        .to_str()
        .unwrap()
        .contains("page=2"));
    document(listed, "issues/list-issue-field-values-for-issue", 200).await;
    let public = document(
        request(&router, "GET", &endpoint, None, Value::Null).await,
        "issues/list-issue-field-values-for-issue",
        200,
    )
    .await;
    assert_eq!(public.as_array().unwrap().len(), 5);
    assert!(!public.to_string().contains("private-field-secret"));
    let outsider_html = outsider.page_ok(&page).await;
    assert!(!outsider_html.contains("private-field-secret"));
    assert!(!outsider_html.contains("Internal"));
    assert!(!outsider_html.contains("Manage owner attributes"));
    let denied = outsider
        .form_reply(
            &action,
            &[
                ("repository_id", &repository_id),
                ("item_id", &item_id),
                ("owner_key", &owner_key),
                ("action", "configure"),
                ("kind", "type"),
                ("name", "Unauthorized"),
                ("is_enabled", "true"),
            ],
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    document(
        request(
            &router,
            "POST",
            &endpoint,
            Some(other),
            json!({"issue_field_values":[]}),
        )
        .await,
        "issues/add-issue-field-values",
        403,
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &endpoint,
            Some(read),
            json!({"issue_field_values":[]}),
        )
        .await,
        "issues/add-issue-field-values",
        403,
    )
    .await;
    // Invalid batches never partially overwrite a previously valid value.
    for invalid in [
        json!({"field_id":field("number").id,"value":"not a number"}),
        json!({"field_id":field("date").id,"value":"2026-02-30"}),
        json!({"field_id":field("single_select").id,"value":"Missing"}),
        json!({"field_id":field("multi_select").id,"value":["High","High"]}),
    ] {
        document(request(&router,"POST",&endpoint,Some(write),json!({"issue_field_values":[{"field_id":field("text").id,"value":"must not save"},invalid]})).await,"issues/add-issue-field-values",422).await;
    }
    let current = document(
        request(&router, "GET", &endpoint, Some(read), Value::Null).await,
        "issues/list-issue-field-values-for-issue",
        200,
    )
    .await;
    assert_eq!(current, values);
    // Renaming an option preserves its identity and existing selections.
    let single = field("single_select");
    let option = &single.options[0];
    let option_name = format!("option_{}_name", option.id);
    let option_color = format!("option_{}_color", option.id);
    let renamed = browser
        .form_reply(
            &action,
            &[
                ("action", "configure"),
                ("kind", "field"),
                ("repository_id", &repository_id),
                ("item_id", &item_id),
                ("owner_key", &owner_key),
                (
                    "expected_revision",
                    &db::issue_attributes::revision(single).unwrap(),
                ),
                ("attribute_id", &single.id.to_string()),
                ("expected_updated", &single.updated_at.to_rfc3339()),
                ("name", "single_select"),
                ("data_type", "single_select"),
                ("visibility", "all"),
                (&option_name, "Urgent"),
                (&option_color, "orange"),
            ],
        )
        .await;
    assert_eq!(renamed.status, StatusCode::SEE_OTHER, "{}", renamed.text());
    let current = document(
        request(&router, "GET", &endpoint, Some(read), Value::Null).await,
        "issues/list-issue-field-values-for-issue",
        200,
    )
    .await;
    let selected = current
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["issue_field_id"] == single.id)
        .unwrap();
    assert_eq!(selected["single_select_option"]["id"], option.id);
    assert_eq!(selected["single_select_option"]["name"], "Urgent");
    document(
        request(
            &router,
            "POST",
            &endpoint,
            Some(write),
            json!({"issue_field_values":[{"field_id":field("text").id,"value":"new text"}]}),
        )
        .await,
        "issues/add-issue-field-values",
        200,
    )
    .await;
    let current = document(
        request(
            &router,
            "PUT",
            &endpoint,
            Some(write),
            json!({"issue_field_values":[{"field_id":field("text").id,"value":"new text"}]}),
        )
        .await,
        "issues/set-issue-field-values",
        200,
    )
    .await;
    assert_eq!(current.as_array().unwrap().len(), 1);
    assert_eq!(current[0]["node_id"], text_node);
    let deletion = format!("{endpoint}/{}", field("text").id);
    let deleted = request(&router, "DELETE", &deletion, Some(write), Value::Null).await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(axum::body::to_bytes(deleted.into_body(), 1024)
        .await
        .unwrap()
        .is_empty());
    document(
        request(&router, "DELETE", &deletion, Some(write), Value::Null).await,
        "issues/delete-issue-field-value",
        404,
    )
    .await;
    let reset = document(
        request(
            &router,
            "PATCH",
            &typed_path,
            Some(write),
            json!({"type":null}),
        )
        .await,
        "issues/update",
        200,
    )
    .await;
    assert!(reset["type"].is_null());
    document(
        request(
            &router,
            "PATCH",
            &typed_path,
            Some(write),
            json!({"type":"Bug"}),
        )
        .await,
        "issues/update",
        200,
    )
    .await;
    let bug_current = db::issue_attributes::catalog(f.pool(), repo.id, Some(alice), "type")
        .await
        .unwrap()
        .into_iter()
        .find(|attribute| attribute.id == bug)
        .unwrap();
    let bug_updated = bug_current.updated_at.to_rfc3339();
    let disabled = browser
        .form_reply(
            &action,
            &[
                ("action", "configure"),
                ("kind", "type"),
                ("repository_id", &repository_id),
                ("item_id", &item_id),
                ("owner_key", &owner_key),
                (
                    "expected_revision",
                    &db::issue_attributes::revision(&bug_current).unwrap(),
                ),
                ("attribute_id", &bug.to_string()),
                ("expected_updated", &bug_updated),
                ("name", "Bug"),
                ("color", "red"),
            ],
        )
        .await;
    assert_eq!(
        disabled.status,
        StatusCode::SEE_OTHER,
        "{}",
        disabled.text()
    );
    let enabled = document(
        request(
            &router,
            "GET",
            &format!("{base}/issue-types"),
            Some(read),
            Value::Null,
        )
        .await,
        "repos/list-issue-types",
        200,
    )
    .await;
    assert!(enabled.as_array().unwrap().is_empty());
    let kept = document(
        request(&router, "GET", &typed_path, Some(read), Value::Null).await,
        "issues/get",
        200,
    )
    .await;
    assert_eq!(kept["type"]["id"], bug);
    assert_eq!(kept["type"]["is_enabled"], false);
    document(
        request(
            &router,
            "GET",
            &format!("{typed_path}/timeline"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-events-for-timeline",
        200,
    )
    .await;
    document(request(&router,"POST",&endpoint,Some(write),json!({"issue_field_values":[{"field_id":field("Internal").id,"value":"new private value"}]})).await,"issues/add-issue-field-values",200).await;
    let deleted = browser
        .form_reply(
            &action,
            &[
                ("repository_id", &repository_id),
                ("item_id", &item_id),
                ("owner_key", &owner_key),
                ("action", "delete_definition"),
                ("attribute_id", &field("Internal").id.to_string()),
                ("confirm", "true"),
            ],
        )
        .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER, "{}", deleted.text());
    assert!(
        db::issue_attributes::values(f.pool(), repo.id, issue.id, Some(alice))
            .await
            .unwrap()
            .is_empty()
    );
    let html = browser.page_ok(&page).await;
    assert!(html.contains("Manage owner attributes"));
    assert!(html.contains("Issue attributes"));
    let bad_csrf = browser
        .exchange(
            "POST",
            &action,
            "application/x-www-form-urlencoded",
            b"action=set_type&type_id=".to_vec(),
        )
        .await;
    assert_eq!(bad_csrf.status, StatusCode::FORBIDDEN);
    // An organization admin cannot change organization-wide definitions through a repository.
    let admin = f.actor("carol").await;
    let added = db::organizations::add_organization_member(
        f.pool(),
        alice,
        org.id,
        admin,
        db::models::OrgRole::Admin,
    )
    .await
    .unwrap();
    assert!(matches!(
        added,
        db::organizations::OrganizationOutcome::Applied(_)
    ));
    let refused = db::issue_attributes::configure(
        f.pool(),
        repo.id,
        admin,
        &db::issue_attributes::Definition {
            id: None,
            expected_updated: None,
            kind: "field".into(),
            name: "Admin field".into(),
            description: None,
            data_type: Some("text".into()),
            visibility: "all".into(),
            is_enabled: true,
            color: None,
            options: Vec::new(),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        refused.downcast_ref::<db::issue_attributes::Refusal>(),
        Some(db::issue_attributes::Refusal::Denied)
    ));
    f.finish().await;
}
#[tokio::test]
async fn github_issue_attributes_sqlite() {
    attributes(false).await;
}
#[tokio::test]
async fn github_issue_attributes_postgres() {
    attributes(true).await;
}

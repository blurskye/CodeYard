//! Projects v2 through the real router on both engines.
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};

use codeyard_db_repos::projects;
use codeyard_db_repos::projects::Owner;
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn request(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    input: Value,
    operation: &str,
    expected: u16,
) -> Value {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", token);
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::from(input.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        status.as_u16(),
        expected,
        "{method} {path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    if status == StatusCode::NO_CONTENT {
        assert!(bytes.is_empty());
        return Value::Null;
    }
    let body = serde_json::from_slice(&bytes).unwrap();
    assert_github_schema(operation, expected, &body);
    body
}

async fn fields(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("projects-alice").await;
    let bob = f.actor("projects-bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), alice).await;
    let mut credentials = Vec::new();
    for scope in ["read", "write"] {
        let token = tokens::create(
            f.pool(),
            alice,
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
    f.execute(
        "INSERT INTO organizations(name) VALUES('projects-org')",
        &[],
    )
    .await;
    let org = f
        .scalar(
            "SELECT id FROM organizations WHERE name='projects-org'",
            &[],
        )
        .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner'),($1,$3,'member')",
        &[org, alice, bob],
    )
    .await;
    f.execute("INSERT INTO repositories(owner_user_id,name,disk_path) VALUES($1,'work','/project-work.git')", &[alice]).await;
    let repo = f
        .scalar(
            "SELECT id FROM repositories WHERE name='work' AND owner_user_id=$1",
            &[alice],
        )
        .await;
    f.execute(
        "INSERT INTO issues(repo_id,number,author_id,title) VALUES($1,1,$2,'Issue work')",
        &[repo, alice],
    )
    .await;
    f.execute(
        "UPDATE repositories SET is_private=true WHERE id=$1",
        &[repo],
    )
    .await;
    let issue = f
        .scalar(
            "SELECT id FROM issues WHERE repo_id=$1 AND number=1",
            &[repo],
        )
        .await;
    f.execute("INSERT INTO pull_requests(repo_id,number,author_id,title,head_repo_id,head_branch,head_sha,base_branch,base_sha) VALUES($1,2,$2,'Pull work',$1,'topic','abc','main','def')", &[repo,alice]).await;
    let pull = f
        .scalar(
            "SELECT id FROM pull_requests WHERE repo_id=$1 AND number=2",
            &[repo],
        )
        .await;
    for (owner, prefix, area) in [
        (Owner::User(alice), "/api/v3/users/projects-alice", "user"),
        (Owner::Organization(org), "/api/v3/orgs/projects-org", "org"),
    ] {
        let project = projects::create(
            f.pool(),
            alice,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                &session.session,
            ),
            owner,
            "Roadmap",
            Some("Plans"),
            false,
        )
        .await
        .unwrap();
        assert!(projects::create(
            f.pool(),
            bob,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                &crate::support::live_session(f.pool(), bob).await
            ),
            owner,
            "Denied",
            None,
            true
        )
        .await
        .is_err());
        let base = format!("{prefix}/projectsV2/{}", project.number);
        for token in [None, Some(credentials[0].as_str())] {
            let expected = if token.is_some() { 200 } else { 404 };
            let body = request(
                &router,
                "GET",
                &base,
                token,
                Value::Null,
                &format!("projects/get-for-{area}"),
                expected,
            )
            .await;
            if expected == 200 {
                assert_eq!(body["title"], "Roadmap");
                assert_eq!(body["owner"]["id"], owner.parts().1);
            }
        }
        let list = request(
            &router,
            "GET",
            &format!("{prefix}/projectsV2?per_page=1"),
            Some(&credentials[0]),
            Value::Null,
            &format!("projects/list-for-{area}"),
            200,
        )
        .await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        let fields = request(
            &router,
            "GET",
            &format!("{base}/fields"),
            Some(&credentials[0]),
            Value::Null,
            &format!("projects/list-fields-for-{area}"),
            200,
        )
        .await;
        assert_eq!(fields[0]["data_type"], "title");
        assert_eq!(fields[1]["options"][0]["name"]["raw"], "Todo");
        let id = fields[1]["id"].as_i64().unwrap();
        request(
            &router,
            "GET",
            &format!("{base}/fields/{id}"),
            Some(&credentials[0]),
            Value::Null,
            &format!("projects/get-field-for-{area}"),
            200,
        )
        .await;
        let operation = format!("projects/add-field-for-{area}");
        request(
            &router,
            "POST",
            &format!("{base}/fields"),
            Some(&credentials[0]),
            json!({"name":"Effort","data_type":"number"}),
            &operation,
            403,
        )
        .await;
        for input in [
            json!({"name":"Effort","data_type":"number"}),
            json!({"name":"Notes","data_type":"text"}),
            json!({"name":"Due","data_type":"date"}),
            json!({"name":"Priority","data_type":"single_select","single_select_options":[{"name":"<High>","color":"RED"}]}),
            json!({"name":"Sprint","data_type":"iteration","iteration_configuration":{"start_date":"2026-01-05","duration":14,"iterations":[{"title":"Sprint 1","start_date":"2026-01-05","duration":14}]}}),
        ] {
            request(
                &router,
                "POST",
                &format!("{base}/fields"),
                Some(&credentials[1]),
                input,
                &operation,
                201,
            )
            .await;
        }
        for input in [
            json!({"name":"Effort","data_type":"number"}),
            json!({"name":"","data_type":"text"}),
            json!({"name":"Empty","data_type":"single_select","single_select_options":[]}),
            json!({"issue_field_id":1}),
        ] {
            request(
                &router,
                "POST",
                &format!("{base}/fields"),
                Some(&credentials[1]),
                input,
                &operation,
                422,
            )
            .await;
        }
        items(&router, &base, area, alice, &credentials, issue, pull, id).await;
        if area == "org" {
            // An organization token cannot reach alice's private repository, so the project
            // lists its issue and pull request with their content redacted and cannot add them.

            let created = codeyard_identity::fine_grained::create(
                f.pool(),
                alice,
                Some(&session.session),
                codeyard_identity::fine_grained::Request {
                    name: "projects".into(),
                    description: "Project reader".into(),
                    owner: codeyard_identity::fine_grained::Owner::Organization(org),
                    access: codeyard_identity::fine_grained::Access::All,
                    repositories: vec![],
                    permissions: json!({"organization_projects":"write","issues":"read","pull_requests":"read"})
                        .as_object()
                        .unwrap()
                        .clone(),
                    expires_at: Some(chrono::Utc::now() + chrono::Duration::days(30)),
                },
            )
            .await
            .unwrap();
            codeyard_db_identity::fine_grained_tokens::review(
                f.pool(),
                org,
                alice,
                &[created.id],
                "pending",
                "approved",
                None,
            )
            .await
            .unwrap();
            let fine = format!("Bearer {}", created.value);
            let listed = request(
                &router,
                "GET",
                &format!("{base}/items"),
                Some(&fine),
                Value::Null,
                "projects/list-items-for-org",
                200,
            )
            .await;
            let linked: Vec<_> = listed
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["content_type"] != "DraftIssue")
                .collect();
            assert_eq!(linked.len(), 2, "{listed}");
            assert!(
                linked.iter().all(|item| item["content"].is_null()),
                "{listed}"
            );
            request(
                &router,
                "POST",
                &format!("{base}/items"),
                Some(&fine),
                json!({"type":"Issue","id":issue}),
                "projects/add-item-for-org",
                404,
            )
            .await;
        }
        projects::save(
            f.pool(),
            alice,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                &session.session,
            ),
            project.id,
            &projects::Settings {
                title: Some("Published".into()),
                description: None,
                public: Some(true),
                closed: Some(true),
            },
        )
        .await
        .unwrap();
        // An update that names no field keeps every stored value.
        let unchanged = projects::save(
            f.pool(),
            alice,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                &session.session,
            ),
            project.id,
            &projects::Settings {
                title: None,
                description: None,
                public: None,
                closed: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(unchanged.title, "Published");
        assert!(unchanged.public && unchanged.closed_at.is_some());
        let body = request(
            &router,
            "GET",
            &base,
            None,
            Value::Null,
            &format!("projects/get-for-{area}"),
            200,
        )
        .await;
        assert_eq!(body["state"], "closed");
        let hidden = request(
            &router,
            "GET",
            &format!("{base}/items"),
            None,
            Value::Null,
            &format!("projects/list-items-for-{area}"),
            200,
        )
        .await;
        assert_eq!(
            hidden,
            json!([]),
            "a public project must not disclose private repository items"
        );
        projects::delete(
            f.pool(),
            alice,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                &session.session,
            ),
            project.id,
        )
        .await
        .unwrap();
        request(
            &router,
            "GET",
            &base,
            Some(&credentials[1]),
            Value::Null,
            &format!("projects/get-for-{area}"),
            404,
        )
        .await;
        let next = projects::create(
            f.pool(),
            alice,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                &session.session,
            ),
            owner,
            "Next",
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(next.number, project.number + 1);
    }
    native_pages(&f, &router, &session, alice, bob).await;
    codeyard_db_repos::projects::list(
        f.pool(),
        None,
        &projects::ListFilter {
            owner: Some(Owner::User(alice)),
            ..Default::default()
        },
        0,
        50,
    )
    .await
    .unwrap();
    f.finish().await;
}

async fn native_pages(f: &Database, router: &Router, alice: &Client, actor: i64, outsider: i64) {
    let bob = Client::login(router, f.pool(), outsider).await;
    let reply = alice
        .form_reply(
            "/users/projects-alice/projects",
            &[("title", "Native planning")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let path = reply.location.unwrap();
    let html = alice.page_ok(&path).await;
    assert!(html.contains("Native planning") && html.contains("Add draft"));
    assert_eq!(bob.page(&path).await.0, StatusCode::NOT_FOUND);
    let reply = alice
        .form_reply(
            &path,
            &[
                ("action", "draft"),
                ("title", "Native draft"),
                ("body", "Notes"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert!(alice
        .page_ok(&format!("{path}/views/2"))
        .await
        .contains("Native draft"));
    let settings = format!("{path}/settings");
    let html = alice.page_ok(&settings).await;
    assert!(html.contains("Project settings") && html.contains("Link a repository"));
    let reply = alice
        .form_reply(
            &path,
            &[
                ("action", "add-field"),
                ("name", "Estimate"),
                ("data_type", "number"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let number = path.rsplit('/').next().unwrap().parse().unwrap();
    let project = projects::get(f.pool(), Some(actor), Owner::User(actor), number)
        .await
        .unwrap();
    let fields = projects::fields(f.pool(), Some(actor), project.id)
        .await
        .unwrap();
    let field = fields.iter().find(|f| f.name == "Estimate").unwrap();
    let item = projects::items(f.pool(), Some(actor), project.id)
        .await
        .unwrap()
        .remove(0);
    let key = format!("value_{}", field.id);
    // The editor also sends the value it showed, which for the new field is empty.
    let was = format!("was_{}", field.id);
    let item_id = item.id.to_string();
    let reply = alice
        .form_reply(
            &path,
            &[
                ("action", "update-item"),
                ("item_id", &item_id),
                (&key, "8.5"),
                (&was, "\"\""),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let html = alice.page_ok(&format!("{path}?q=Estimate:8.5")).await;
    assert!(html.contains("Native draft") && html.contains("8.5"));
    let reply = alice
        .form_reply(
            &path,
            &[
                ("action", "link-repository"),
                ("owner", "projects-alice"),
                ("repo", "work"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let repo = f
        .scalar(
            "SELECT id FROM repositories WHERE name='work' AND owner_user_id=$1",
            &[actor],
        )
        .await;
    assert!(projects::list(
        f.pool(),
        Some(actor),
        &projects::ListFilter {
            repo: Some(repo),
            ..Default::default()
        },
        0,
        100
    )
    .await
    .unwrap()
    .1
    .iter()
    .any(|p| p.id == project.id));
    let issue_page = "/projects-alice/work/issues/1";
    let html = alice.page_ok(issue_page).await;
    assert!(html.contains("Projects") && html.contains("Native planning"));
    let reply = alice
        .form_reply(
            &path,
            &[
                ("action", "add-item"),
                ("kind", "Issue"),
                ("owner", "projects-alice"),
                ("repo", "work"),
                ("number", "1"),
                ("return_to", issue_page),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert_eq!(reply.location.as_deref(), Some(issue_page));
    let sidebar_item = projects::items(f.pool(), Some(actor), project.id)
        .await
        .unwrap()
        .into_iter()
        .find(|i| i.issue_id.is_some())
        .unwrap();
    let sidebar_id = sidebar_item.id.to_string();
    let reply = alice
        .form_reply(
            &path,
            &[
                ("action", "delete-item"),
                ("item_id", &sidebar_id),
                ("return_to", issue_page),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let reply = alice
        .form_reply(
            &path,
            &[
                ("action", "save"),
                ("title", "Published native"),
                ("description", "A plan"),
                ("public", "on"),
                ("closed", "on"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert!(bob.page_ok(&path).await.contains("Published native"));
    let reply = bob
        .form_reply(&path, &[("action", "draft"), ("title", "Denied")])
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let (status, page) = alice.model(&path).await;
    assert_eq!(status, StatusCode::OK);
    let serialized = serde_json::to_string(&page).unwrap();
    let restored: codeyard_webui::app::Page = serde_json::from_str(&serialized).unwrap();
    assert!(matches!(restored, codeyard_webui::app::Page::Projects(_)));
    let bad_csrf = alice
        .exchange(
            "POST",
            &path,
            "application/x-www-form-urlencoded",
            b"action=delete&csrf_token=bad".to_vec(),
        )
        .await;
    assert_eq!(bad_csrf.status, StatusCode::FORBIDDEN);
    let reply = alice.form_reply(&path, &[("action", "delete")]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert_eq!(alice.page(&path).await.0, StatusCode::NOT_FOUND);
}

#[expect(
    clippy::too_many_arguments,
    reason = "The helper walks one project's items with the fixture IDs the test created."
)]
async fn items(
    router: &Router,
    base: &str,
    area: &str,
    user: i64,
    credentials: &[String],
    issue: i64,
    pull: i64,
    status_field: i64,
) {
    let draft_path = if area == "org" {
        format!("{base}/drafts")
    } else {
        format!(
            "/api/v3/user/{user}/projectsV2/{}/drafts",
            base.rsplit('/').next().unwrap()
        )
    };
    let draft_op = if area == "org" {
        "projects/create-draft-item-for-org"
    } else {
        "projects/create-draft-item-for-authenticated-user"
    };
    request(
        router,
        "POST",
        &draft_path,
        Some(&credentials[0]),
        json!({"title":"Draft"}),
        draft_op,
        403,
    )
    .await;
    let draft = request(
        router,
        "POST",
        &draft_path,
        Some(&credentials[1]),
        json!({"title":"Draft work","body":"Remember this"}),
        draft_op,
        201,
    )
    .await;
    assert_eq!(draft["content"]["body"], "Remember this");
    let draft_id = draft["id"].as_i64().unwrap();
    let add = format!("projects/add-item-for-{area}");
    for input in [
        json!({"type":"Issue","id":issue}),
        json!({"type":"PullRequest","id":pull}),
        json!({"type":"Issue","owner":"projects-alice","repo":"work","number":1}),
    ] {
        request(
            router,
            "POST",
            &format!("{base}/items"),
            Some(&credentials[1]),
            input,
            &add,
            201,
        )
        .await;
    }
    for input in [
        json!({"type":"Wrong","id":issue}),
        json!({"type":"Issue"}),
        json!({"type":"Issue","id":issue,"owner":"projects-alice","repo":"work","number":1}),
    ] {
        request(
            router,
            "POST",
            &format!("{base}/items"),
            Some(&credentials[1]),
            input,
            &add,
            422,
        )
        .await;
    }
    request(
        router,
        "POST",
        &format!("{base}/items"),
        Some(&credentials[1]),
        json!({"type":"Issue","id":i64::MAX}),
        &add,
        404,
    )
    .await;
    let list = format!("projects/list-items-for-{area}");
    let body = request(
        router,
        "GET",
        &format!("{base}/items"),
        Some(&credentials[0]),
        Value::Null,
        &list,
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 3, "idempotent adds");
    let get = if area == "org" {
        "projects/get-org-item"
    } else {
        "projects/get-user-item"
    };
    let body = request(
        router,
        "GET",
        &format!("{base}/items/{draft_id}"),
        Some(&credentials[0]),
        Value::Null,
        get,
        200,
    )
    .await;
    assert_eq!(body["content"]["title"], "Draft work");
    let update = format!("projects/update-item-for-{area}");
    request(
        router,
        "PATCH",
        &format!("{base}/items/{draft_id}"),
        Some(&credentials[0]),
        json!({"fields":[]}),
        &update,
        403,
    )
    .await;
    request(
        router,
        "PATCH",
        &format!("{base}/items/{draft_id}"),
        Some(&credentials[1]),
        json!({"fields":[{"id":status_field,"value":"missing"}]}),
        &update,
        422,
    )
    .await;
    request(
        router,
        "PATCH",
        &format!("{base}/items/{draft_id}"),
        Some(&credentials[1]),
        json!({"fields":[{"id":i64::MAX,"value":"missing"}]}),
        &update,
        422,
    )
    .await;
    let fields = request(
        router,
        "GET",
        &format!("{base}/fields"),
        Some(&credentials[0]),
        Value::Null,
        &format!("projects/list-fields-for-{area}"),
        200,
    )
    .await;
    let option = fields[1]["options"][1]["id"].clone();
    let effort = fields
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "Effort")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    request(
        router,
        "PATCH",
        &format!("{base}/items/{draft_id}"),
        Some(&credentials[1]),
        json!({"fields":[{"id":status_field,"value":option},{"id":effort,"value":3.5}]}),
        &update,
        200,
    )
    .await;
    let create_view = format!("projects/create-view-for-{area}");
    let view_path = if area == "org" {
        format!("{base}/views")
    } else {
        format!(
            "/api/v3/users/{user}/projectsV2/{}/views",
            base.rsplit('/').next().unwrap()
        )
    };
    let body = request(router,"POST",&view_path,Some(&credentials[1]),json!({"name":"Active","layout":"board","filter":"status:\"In progress\"","vertical_group_by":[status_field],"sort_by":[[effort,"desc"]]}),&create_view,201).await;
    let number = body["number"].as_i64().unwrap();
    let view_list = format!("projects/list-view-items-for-{area}");
    let body = request(
        router,
        "GET",
        &format!("{base}/views/{number}/items"),
        Some(&credentials[0]),
        Value::Null,
        &view_list,
        200,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], draft_id);
    request(
        router,
        "POST",
        &view_path,
        Some(&credentials[1]),
        json!({"name":"Bad","layout":"board","sort_by":[[effort,"sideways"]]}),
        &create_view,
        422,
    )
    .await;
    request(
        router,
        "GET",
        &format!("{base}/views/999/items"),
        Some(&credentials[0]),
        Value::Null,
        &view_list,
        404,
    )
    .await;
    request(
        router,
        "PATCH",
        &format!("{base}/items/{draft_id}"),
        Some(&credentials[1]),
        json!({"fields":[{"id":status_field,"value":null}]}),
        &update,
        200,
    )
    .await;
    let delete = format!("projects/delete-item-for-{area}");
    request(
        router,
        "DELETE",
        &format!("{base}/items/{draft_id}"),
        Some(&credentials[0]),
        Value::Null,
        &delete,
        403,
    )
    .await;
    request(
        router,
        "DELETE",
        &format!("{base}/items/{draft_id}"),
        Some(&credentials[1]),
        Value::Null,
        &delete,
        204,
    )
    .await;
    request(
        router,
        "GET",
        &format!("{base}/items/{draft_id}"),
        Some(&credentials[0]),
        Value::Null,
        get,
        404,
    )
    .await;
}

#[tokio::test]
async fn projects_fields_sqlite() {
    fields(false).await;
}
#[tokio::test]
async fn projects_fields_postgres() {
    fields(true).await;
}

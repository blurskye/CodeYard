use super::github::{document, request};
use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::{http::StatusCode, Router};

use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
async fn propose(
    router: &Router,
    url: &str,
    token: &str,
    action: &str,
    target: i64,
    value: Value,
    rationale: &str,
) -> Value {
    let response=request(router,"POST",url,Some(token),json!({"action":action,"target_id":target,"target_value":value,"rationale":rationale,"confidence":"HIGH"})).await;
    let status = response.status();
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}
async fn suggestions(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let carol = f.actor("carol").await;
    let dave = f.actor("dave").await;
    let org = match codeyard_db_orgs::organizations::profiles::create_organization(
        f.pool(),
        alice,
        &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
            "collective",
            codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                .unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    {
        codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(org) => org,
        _ => panic!("create organization"),
    };
    use codeyard_db_repos::repositories::creation::{RepositoryCreationOwner, RepositoryTarget};
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_app::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner {
            actor_id: alice,
            target: RepositoryTarget::Organization(org.id),
        },
        "proj",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        codeyard_db_core::models::AccessLevel::Triage,
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        carol,
        repo.id,
        codeyard_db_core::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let issue =
        codeyard_db_repos::issues::create_issue(f.pool(), repo.id, alice, "Suggestions", "")
            .await
            .unwrap();
    let duplicate =
        codeyard_db_repos::issues::create_issue(f.pool(), repo.id, alice, "Canonical issue", "")
            .await
            .unwrap();
    let label =
        codeyard_db_repos::labels::create_label(f.pool(), alice, repo.id, "bug", "ff0000", None)
            .await
            .unwrap();
    let race_label =
        codeyard_db_repos::labels::create_label(f.pool(), alice, repo.id, "race", "ff0000", None)
            .await
            .unwrap();
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), alice).await;
    let triage = Client::login(&router, f.pool(), bob).await;
    let agent = Client::login(&router, f.pool(), carol).await;
    let outsider = Client::login(&router, f.pool(), dave).await;
    let mut credentials = Vec::new();
    for (actor, scope, session) in [
        (alice, "write", browser.session.as_str()),
        (bob, "write", triage.session.as_str()),
        (carol, "write", agent.session.as_str()),
        (alice, "read", browser.session.as_str()),
        (dave, "write", outsider.session.as_str()),
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
    let (write, review, submit, read, other) = (
        &credentials[0],
        &credentials[1],
        &credentials[2],
        &credentials[3],
        &credentials[4],
    );
    let base = "/api/v3/repos/collective/proj";
    let path = format!("{base}/issues/{}", issue.number);
    let endpoint = format!("{path}/suggestions");
    let page = format!("/collective/proj/issues/{}", issue.number);
    let form = format!("{page}/suggestions");
    let repository_id = repo.id.to_string();
    let issue_id = issue.id.to_string();
    let agent_api = format!(
        "/api/repos/collective/proj/issues/{}/suggestions",
        issue.number
    );
    document(
        request(&router, "GET", &endpoint, Some(write), Value::Null).await,
        "issues/list-suggestions",
        404,
    )
    .await;
    let enabled = browser
        .form_reply(
            &form,
            &[
                ("repository_id", &repository_id),
                ("issue_id", &issue_id),
                ("action", "set_enabled"),
                ("enabled", "true"),
            ],
        )
        .await;
    assert_eq!(enabled.status, StatusCode::SEE_OTHER, "{}", enabled.text());
    // A stale proposal must not publish its rationale on the URL-resolved issue.
    for (old_repo, old_issue) in [(repo.id, duplicate.id), (repo.id + 1, issue.id)] {
        let stale = agent
            .form_reply(
                &form,
                &[
                    ("repository_id", &old_repo.to_string()),
                    ("issue_id", &old_issue.to_string()),
                    ("action", "propose"),
                    ("change", "add_label"),
                    ("target", "bug"),
                    ("rationale", "Private stale proposal"),
                ],
            )
            .await;
        assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.text());
        assert!(stale.text().contains("Private stale proposal"));
    }
    let untouched = document(
        request(&router, "GET", &endpoint, Some(write), Value::Null).await,
        "issues/list-suggestions",
        200,
    )
    .await;
    assert!(untouched.as_array().unwrap().is_empty());
    let attr_form = format!("{page}/attributes");
    for name in ["Bug", "Task"] {
        let created = browser
            .form_reply(
                &attr_form,
                &[
                    ("repository_id", &repository_id),
                    ("item_id", &issue_id),
                    ("owner_key", &format!("org:{}", org.id)),
                    ("action", "configure"),
                    ("kind", "type"),
                    ("name", name),
                    ("is_enabled", "true"),
                ],
            )
            .await;
        assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
    }
    for (name, kind, visibility) in [
        ("Priority", "number", "all"),
        ("Secret", "text", "organization_members_only"),
    ] {
        let created = browser
            .form_reply(
                &attr_form,
                &[
                    ("repository_id", &repository_id),
                    ("item_id", &issue_id),
                    ("owner_key", &format!("org:{}", org.id)),
                    ("action", "configure"),
                    ("kind", "field"),
                    ("name", name),
                    ("data_type", kind),
                    ("visibility", visibility),
                ],
            )
            .await;
        assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
    }
    let types =
        codeyard_db_repos::issue_attributes::catalog(f.pool(), repo.id, Some(alice), "type")
            .await
            .unwrap();
    let bug = types.iter().find(|a| a.name == "Bug").unwrap().id;
    let task = types.iter().find(|a| a.name == "Task").unwrap().id;
    let fields =
        codeyard_db_repos::issue_attributes::catalog(f.pool(), repo.id, Some(alice), "field")
            .await
            .unwrap();
    let priority = fields.iter().find(|a| a.name == "Priority").unwrap().id;
    let secret = fields.iter().find(|a| a.name == "Secret").unwrap().id;
    let bug_proposal = propose(
        &router,
        &agent_api,
        submit,
        "set_type",
        bug,
        Value::Null,
        "Bug report",
    )
    .await;
    let competing = propose(
        &router,
        &agent_api,
        write,
        "set_type",
        task,
        Value::Null,
        "Alternative classification",
    )
    .await;
    let hidden = propose(
        &router,
        &agent_api,
        write,
        "add_field",
        secret,
        json!("private-target-value"),
        "Private field rationale",
    )
    .await;
    let listed = request(
        &router,
        "GET",
        &format!("{endpoint}?per_page=1"),
        Some(review),
        Value::Null,
    )
    .await;
    assert!(listed.headers()["link"]
        .to_str()
        .unwrap()
        .contains("page=2"));
    document(listed, "issues/list-suggestions", 200).await;
    let list = document(
        request(&router, "GET", &endpoint, Some(review), Value::Null).await,
        "issues/list-suggestions",
        200,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert!(!list.to_string().contains("private-target-value"));
    assert_eq!(
        request(&router, "GET", &endpoint, Some(submit), Value::Null)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    document(
        request(&router, "GET", &endpoint, Some(other), Value::Null).await,
        "issues/list-suggestions",
        404,
    )
    .await;
    let approve = |id: &Value| format!("{endpoint}/{id}/approve");
    let dismiss = |id: &Value| format!("{endpoint}/{id}/dismiss");
    document(
        request(
            &router,
            "POST",
            &approve(&hidden["id"]),
            Some(review),
            Value::Null,
        )
        .await,
        "issues/approve-suggestion",
        404,
    )
    .await;
    let approved = document(
        request(
            &router,
            "POST",
            &approve(&bug_proposal["id"]),
            Some(review),
            Value::Null,
        )
        .await,
        "issues/approve-suggestion",
        200,
    )
    .await;
    assert_eq!(approved["state"], "approved");
    assert_eq!(approved["resolved_by"], bob);
    assert!(approved["issue_event_id"].is_i64());
    let dismissed = document(
        request(
            &router,
            "GET",
            &format!("{endpoint}?state=dismissed"),
            Some(write),
            Value::Null,
        )
        .await,
        "issues/list-suggestions",
        200,
    )
    .await;
    assert_eq!(dismissed[0]["id"], competing["id"]);
    let got = document(
        request(&router, "GET", &path, Some(read), Value::Null).await,
        "issues/get",
        200,
    )
    .await;
    assert_eq!(got["type"]["id"], bug);
    let applied = propose(
        &router,
        &agent_api,
        submit,
        "set_type",
        bug,
        Value::Null,
        "Already classified",
    )
    .await;
    assert_eq!(applied["state"], "applied");
    let no_change = propose(
        &router,
        &agent_api,
        submit,
        "add_label",
        label.id,
        Value::Null,
        "Optional label",
    )
    .await;
    let before = codeyard_db_repos::issues::get_issue_by_id(f.pool(), issue.id)
        .await
        .unwrap()
        .unwrap()
        .updated_at;
    let dismissed = document(
        request(
            &router,
            "POST",
            &dismiss(&no_change["id"]),
            Some(review),
            Value::Null,
        )
        .await,
        "issues/dismiss-suggestion",
        200,
    )
    .await;
    assert_eq!(dismissed["state"], "dismissed");
    assert!(dismissed["issue_event_id"].is_null());
    assert_eq!(
        codeyard_db_repos::issues::get_issue_by_id(f.pool(), issue.id)
            .await
            .unwrap()
            .unwrap()
            .updated_at,
        before
    );
    let add_label = propose(
        &router,
        &agent_api,
        submit,
        "add_label",
        label.id,
        Value::Null,
        "Apply label",
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &approve(&add_label["id"]),
            Some(review),
            Value::Null,
        )
        .await,
        "issues/approve-suggestion",
        200,
    )
    .await;
    assert_eq!(
        codeyard_db_repos::labels::get_issue_labels(f.pool(), issue.repo_id, issue.id)
            .await
            .unwrap()
            .len(),
        1
    );
    let add_field = propose(
        &router,
        &agent_api,
        submit,
        "add_field",
        priority,
        json!(3),
        "Priority estimate",
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &approve(&add_field["id"]),
            Some(review),
            Value::Null,
        )
        .await,
        "issues/approve-suggestion",
        403,
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &approve(&add_field["id"]),
            Some(write),
            Value::Null,
        )
        .await,
        "issues/approve-suggestion",
        200,
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &approve(&hidden["id"]),
            Some(write),
            Value::Null,
        )
        .await,
        "issues/approve-suggestion",
        200,
    )
    .await;
    let before =
        codeyard_db_repos::notifications::records::count_unread_notifications(f.pool(), carol)
            .await
            .unwrap();
    let assign = propose(
        &router,
        &agent_api,
        submit,
        "add_assignee",
        carol,
        Value::Null,
        "Assign reporter",
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &approve(&assign["id"]),
            Some(review),
            Value::Null,
        )
        .await,
        "issues/approve-suggestion",
        200,
    )
    .await;
    assert_eq!(
        codeyard_db_repos::assignees::list_assignees(
            f.pool(),
            codeyard_db_repos::assignees::AssignedItem::Issue(issue.id)
        )
        .await
        .unwrap(),
        vec!["carol"]
    );
    assert_eq!(
        codeyard_db_repos::notifications::records::count_unread_notifications(f.pool(), carol)
            .await
            .unwrap(),
        before + 1
    );
    let close = propose(
        &router,
        &agent_api,
        submit,
        "close_issue",
        duplicate.id,
        json!("duplicate"),
        "Duplicate report",
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &approve(&close["id"]),
            Some(review),
            Value::Null,
        )
        .await,
        "issues/approve-suggestion",
        200,
    )
    .await;
    let closed = codeyard_db_repos::issues::get_issue_by_id(f.pool(), issue.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(closed.state, "closed");
    assert_eq!(closed.state_reason.as_deref(), Some("duplicate"));
    assert_eq!(closed.closed_by_id, Some(bob));
    let old = propose(
        &router,
        &agent_api,
        submit,
        "set_type",
        task,
        Value::Null,
        "First rationale",
    )
    .await;
    let refreshed = propose(
        &router,
        &agent_api,
        submit,
        "set_type",
        task,
        Value::Null,
        "Refreshed rationale",
    )
    .await;
    let replaced = document(
        request(
            &router,
            "GET",
            &format!("{endpoint}?state=replaced&action=set_type"),
            Some(review),
            Value::Null,
        )
        .await,
        "issues/list-suggestions",
        200,
    )
    .await;
    assert_eq!(replaced[0]["id"], old["id"]);
    let resolved = triage
        .form_reply(
            &form,
            &[
                ("repository_id", &repository_id),
                ("issue_id", &issue_id),
                ("action", "dismiss"),
                ("suggestion_id", &refreshed["id"].to_string()),
            ],
        )
        .await;
    assert_eq!(
        resolved.status,
        StatusCode::SEE_OTHER,
        "{}",
        resolved.text()
    );
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        dave,
        repo.id,
        codeyard_db_core::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let stale = propose(
        &router,
        &agent_api,
        submit,
        "add_assignee",
        dave,
        Value::Null,
        "Candidate loses access",
    )
    .await;
    codeyard_db_repos::repositories::access::revoke_repo_access(f.pool(), dave, repo.id)
        .await
        .unwrap();
    document(
        request(
            &router,
            "POST",
            &approve(&stale["id"]),
            Some(review),
            Value::Null,
        )
        .await,
        "issues/approve-suggestion",
        422,
    )
    .await;
    let invalidated = document(
        request(
            &router,
            "GET",
            &format!("{endpoint}?state=invalidated"),
            Some(review),
            Value::Null,
        )
        .await,
        "issues/list-suggestions",
        200,
    )
    .await;
    assert_eq!(invalidated[0]["id"], stale["id"]);
    let race = propose(
        &router,
        &agent_api,
        submit,
        "add_label",
        race_label.id,
        Value::Null,
        "Concurrent review",
    )
    .await;
    let yes = approve(&race["id"]);
    let no = dismiss(&race["id"]);
    let (yes, no) = tokio::join!(
        request(&router, "POST", &yes, Some(review), Value::Null),
        request(&router, "POST", &no, Some(write), Value::Null)
    );
    let statuses = [yes.status(), no.status()];
    assert!(
        statuses.contains(&StatusCode::OK) && statuses.contains(&StatusCode::UNPROCESSABLE_ENTITY)
    );
    let accepted = yes.status() == StatusCode::OK;
    for (reply, op) in [
        (yes, "issues/approve-suggestion"),
        (no, "issues/dismiss-suggestion"),
    ] {
        let status = reply.status().as_u16();
        document(reply, op, status).await;
    }
    let labels = codeyard_db_repos::labels::get_issue_labels(f.pool(), issue.repo_id, issue.id)
        .await
        .unwrap();
    assert_eq!(labels.iter().any(|l| l.id == race_label.id), accepted);
    document(
        request(
            &router,
            "POST",
            &approve(&race["id"]),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/approve-suggestion",
        403,
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &format!(
                "{base}/issues/{}/suggestions/{}/approve",
                duplicate.number, race["id"]
            ),
            Some(write),
            Value::Null,
        )
        .await,
        "issues/approve-suggestion",
        404,
    )
    .await;
    let failed = agent
        .form_reply(
            &form,
            &[
                ("repository_id", &repository_id),
                ("issue_id", &issue_id),
                ("action", "propose"),
                ("change", "add_field"),
                ("target", "Priority"),
                ("value", "not a number"),
                ("rationale", "</textarea><script>unsaved()</script>"),
            ],
        )
        .await;
    assert_eq!(
        failed.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        failed.text()
    );
    assert!(failed.text().contains("not a number"));
    assert!(failed.text().contains("preserved draft"));
    assert!(!failed.text().contains("<script>unsaved()</script>"));
    let csrf = browser
        .exchange(
            "POST",
            &form,
            "application/x-www-form-urlencoded",
            b"action=set_enabled&enabled=false".to_vec(),
        )
        .await;
    assert_eq!(csrf.status, StatusCode::FORBIDDEN);
    let html = triage.page_ok(&page).await;
    assert!(html.contains("Issue suggestions"));
    assert!(!html.contains("private-target-value"));
    document(
        request(
            &router,
            "GET",
            &format!("{path}/timeline"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-events-for-timeline",
        200,
    )
    .await;
    let disabled = browser
        .form_reply(
            &form,
            &[
                ("repository_id", &repository_id),
                ("issue_id", &issue_id),
                ("action", "set_enabled"),
            ],
        )
        .await;
    assert_eq!(disabled.status, StatusCode::SEE_OTHER);
    document(
        request(&router, "GET", &endpoint, Some(write), Value::Null).await,
        "issues/list-suggestions",
        404,
    )
    .await;
    f.finish().await;
}
#[tokio::test]
async fn github_suggestions_sqlite() {
    suggestions(false).await;
}
#[tokio::test]
async fn github_suggestions_postgres() {
    suggestions(true).await;
}

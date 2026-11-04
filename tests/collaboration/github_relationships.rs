use super::github::{document, request};
use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::http::StatusCode;

use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
async fn relationships(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (actor, name, private) in [
        (alice, "proj", true),
        (alice, "other", true),
        (bob, "public", false),
    ] {
        repos.push(
            codeyard_app::repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(actor),
                name,
                None,
                private,
                None,
            )
            .await
            .unwrap(),
        );
    }
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        alice,
        repos[2].id,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let mut issues = Vec::new();
    for (repo, title) in [
        (&repos[0], "Parent"),
        (&repos[0], "First child"),
        (&repos[1], "Second child"),
        (&repos[0], "Other parent"),
        (&repos[2], "Public issue"),
    ] {
        issues.push(
            codeyard_db_repos::issues::create_issue(f.pool(), repo.id, repo.owner_id(), title, "")
                .await
                .unwrap(),
        );
    }
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), alice).await;
    let other_browser = Client::login(&router, f.pool(), bob).await;
    let mut credentials = Vec::new();
    for (id, scope, session) in [
        (alice, "write", browser.session.as_str()),
        (alice, "read", browser.session.as_str()),
        (bob, "write", other_browser.session.as_str()),
    ] {
        let token = tokens::create(
            f.pool(),
            id,
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
    let base = "/api/v3/repos/alice/proj";
    let path = |number| format!("{base}/issues/{number}");
    let parent = path(issues[0].number);
    for child in [&issues[1], &issues[2]] {
        let created = document(
            request(
                &router,
                "POST",
                &format!("{parent}/sub_issues"),
                Some(write),
                json!({"sub_issue_id":child.id}),
            )
            .await,
            "issues/add-sub-issue",
            201,
        )
        .await;
        assert_eq!(created["id"], child.id);
    }
    let response = request(
        &router,
        "GET",
        &format!("{parent}/sub_issues?per_page=1"),
        Some(read),
        Value::Null,
    )
    .await;
    assert!(response.headers()["link"]
        .to_str()
        .unwrap()
        .contains("page=2"));
    let listed = document(response, "issues/list-sub-issues", 200).await;
    assert_eq!(listed[0]["id"], issues[1].id);
    let child_parent = document(
        request(
            &router,
            "GET",
            &format!(
                "/api/v3/repos/alice/other/issues/{}/parent",
                issues[2].number
            ),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/get-parent",
        200,
    )
    .await;
    assert_eq!(child_parent["id"], issues[0].id);
    document(
        request(
            &router,
            "PATCH",
            &format!("{parent}/sub_issues/priority"),
            Some(write),
            json!({"sub_issue_id":issues[2].id,"before_id":issues[1].id}),
        )
        .await,
        "issues/reprioritize-sub-issue",
        200,
    )
    .await;
    let listed = document(
        request(
            &router,
            "GET",
            &format!("{parent}/sub_issues"),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-sub-issues",
        200,
    )
    .await;
    assert_eq!(listed[0]["id"], issues[2].id);
    document(
        request(
            &router,
            "POST",
            &format!("{}/sub_issues", path(issues[1].number)),
            Some(write),
            json!({"sub_issue_id":issues[0].id}),
        )
        .await,
        "issues/add-sub-issue",
        422,
    )
    .await;
    let other_parent = path(issues[3].number);
    document(
        request(
            &router,
            "POST",
            &format!("{other_parent}/sub_issues"),
            Some(write),
            json!({"sub_issue_id":issues[1].id}),
        )
        .await,
        "issues/add-sub-issue",
        422,
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &format!("{other_parent}/sub_issues"),
            Some(write),
            json!({"sub_issue_id":issues[1].id,"replace_parent":true}),
        )
        .await,
        "issues/add-sub-issue",
        201,
    )
    .await;
    document(
        request(
            &router,
            "DELETE",
            &format!("{other_parent}/sub_issue"),
            Some(write),
            json!({"sub_issue_id":issues[1].id}),
        )
        .await,
        "issues/remove-sub-issue",
        200,
    )
    .await;
    document(
        request(
            &router,
            "GET",
            &format!("{}/parent", path(issues[1].number)),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/get-parent",
        404,
    )
    .await;
    for (suffix, add, list, remove) in [
        (
            "dependencies/blocked_by",
            "issues/add-blocked-by-dependency",
            "issues/list-dependencies-blocked-by",
            "issues/remove-dependency-blocked-by",
        ),
        (
            "relates_to",
            "issues/add-relates-to",
            "issues/list-relates-to",
            "issues/remove-relates-to",
        ),
    ] {
        for _ in 0..2 {
            document(
                request(
                    &router,
                    "POST",
                    &format!("{parent}/{suffix}"),
                    Some(write),
                    json!({"issue_id":issues[1].id}),
                )
                .await,
                add,
                201,
            )
            .await;
        }
        let values = document(
            request(
                &router,
                "GET",
                &format!("{parent}/{suffix}"),
                Some(read),
                Value::Null,
            )
            .await,
            list,
            200,
        )
        .await;
        assert_eq!(values.as_array().unwrap().len(), 1);
        document(
            request(
                &router,
                "DELETE",
                &format!("{parent}/{suffix}/{}", issues[1].id),
                Some(write),
                Value::Null,
            )
            .await,
            remove,
            200,
        )
        .await;
    }
    document(
        request(
            &router,
            "POST",
            &format!("{parent}/dependencies/blocked_by"),
            Some(write),
            json!({"issue_id":issues[1].id}),
        )
        .await,
        "issues/add-blocked-by-dependency",
        201,
    )
    .await;
    let blocking = document(
        request(
            &router,
            "GET",
            &format!("{}/dependencies/blocking", path(issues[1].number)),
            Some(read),
            Value::Null,
        )
        .await,
        "issues/list-dependencies-blocking",
        200,
    )
    .await;
    assert_eq!(blocking[0]["id"], issues[0].id);
    document(
        request(
            &router,
            "POST",
            &format!("{}/dependencies/blocked_by", path(issues[1].number)),
            Some(write),
            json!({"issue_id":issues[0].id}),
        )
        .await,
        "issues/add-blocked-by-dependency",
        422,
    )
    .await;
    let got = document(
        request(&router, "GET", &parent, Some(read), Value::Null).await,
        "issues/get",
        200,
    )
    .await;
    assert_eq!(got["sub_issues_summary"]["total"], 1);
    assert_eq!(got["issue_dependencies_summary"]["blocked_by"], 1);
    document(
        request(
            &router,
            "POST",
            &format!("{parent}/sub_issues"),
            Some(read),
            json!({"sub_issue_id":issues[1].id}),
        )
        .await,
        "issues/add-sub-issue",
        403,
    )
    .await;
    document(
        request(
            &router,
            "GET",
            &format!("{parent}/sub_issues"),
            Some(other),
            Value::Null,
        )
        .await,
        "issues/list-sub-issues",
        404,
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &format!("{parent}/sub_issues"),
            Some(write),
            json!({"sub_issue_id":issues[4].id}),
        )
        .await,
        "issues/add-sub-issue",
        422,
    )
    .await;
    let public = format!("/api/v3/repos/bob/public/issues/{}", issues[4].number);
    document(
        request(
            &router,
            "POST",
            &format!("{public}/relates_to"),
            Some(write),
            json!({"issue_id":issues[0].id}),
        )
        .await,
        "issues/add-relates-to",
        201,
    )
    .await;
    let hidden = document(
        request(
            &router,
            "GET",
            &format!("{public}/relates_to"),
            Some(other),
            Value::Null,
        )
        .await,
        "issues/list-relates-to",
        200,
    )
    .await;
    assert!(hidden.as_array().unwrap().is_empty());
    let page = format!("/alice/proj/issues/{}", issues[0].number);
    let html = browser.page_ok(&page).await;
    assert!(html.contains("Issue relationships"));
    assert!(html.contains("Second child"));
    let action = format!("{page}/relationships");
    let reply = browser
        .form_reply(
            &action,
            &[
                ("kind", "related"),
                ("action", "add"),
                ("reference", &format!("#{}", issues[1].number)),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let (_, model) = browser.model(&page).await;
    assert!(serde_json::to_string(&model)
        .unwrap()
        .contains("\"relationships\":["));
    let denied = browser
        .exchange(
            "POST",
            &action,
            "application/x-www-form-urlencoded",
            b"kind=related&action=add&reference=%232".to_vec(),
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    // Disjoint insertions can jointly form a cycle. The native guard admits only one.
    let mut nodes = Vec::new();
    for _ in 0..4 {
        nodes.push(
            codeyard_db_repos::issues::create_issue(f.pool(), repos[0].id, alice, "Concurrent", "")
                .await
                .unwrap(),
        );
    }
    use codeyard_db_identity::relationships::Kind;
    use codeyard_db_identity::relationships::Mutation;
    for (source, target) in [(0, 1), (2, 3)] {
        codeyard_db_identity::relationships::change(
            f.pool(),
            alice,
            repos[0].id,
            nodes[target].id,
            Mutation::Add {
                kind: Kind::BlockedBy,
                other: nodes[source].id,
                replace: false,
            },
        )
        .await
        .unwrap();
    }
    let a = format!("{}/dependencies/blocked_by", path(nodes[2].number));
    let b = format!("{}/dependencies/blocked_by", path(nodes[0].number));
    let (a, b) = tokio::join!(
        request(
            &router,
            "POST",
            &a,
            Some(write),
            json!({"issue_id":nodes[1].id})
        ),
        request(
            &router,
            "POST",
            &b,
            Some(write),
            json!({"issue_id":nodes[3].id})
        )
    );
    let statuses = [a.status(), b.status()];
    assert!(
        statuses.contains(&StatusCode::CREATED)
            && statuses.contains(&StatusCode::UNPROCESSABLE_ENTITY)
    );
    for response in [a, b] {
        let status = response.status().as_u16();
        document(response, "issues/add-blocked-by-dependency", status).await;
    }
    f.execute(
        "UPDATE repositories SET is_archived=true WHERE id=$1",
        &[repos[0].id],
    )
    .await;
    document(
        request(
            &router,
            "POST",
            &format!("{parent}/sub_issues"),
            Some(write),
            json!({"sub_issue_id":issues[1].id}),
        )
        .await,
        "issues/add-sub-issue",
        403,
    )
    .await;
    f.finish().await;
}
#[tokio::test]
async fn github_relationships_sqlite() {
    relationships(false).await;
}
#[tokio::test]
async fn github_relationships_postgres() {
    relationships(true).await;
}

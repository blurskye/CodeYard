use super::github::{document, request};
use crate::support::database::Database;
use crate::support::http_client::Client;
use codeyard::backend::db;
use codeyard::backend::identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::Value;

async fn lists(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let org = match db::organizations::create_organization(
        f.pool(),
        alice,
        &db::organizations::NewOrganization::new(
            "collective",
            db::organizations::OrganizationProfile::new(Some("Collective"), None).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    {
        db::organizations::OrganizationOutcome::Applied(org) => org,
        _ => panic!("organization creation"),
    };
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    use db::repositories::creation::{RepositoryCreationOwner, RepositoryTarget};
    for (owner, name, private) in [
        (RepositoryCreationOwner::personal(alice), "proj", true),
        (RepositoryCreationOwner::personal(bob), "other", false),
        (
            RepositoryCreationOwner {
                actor_id: alice,
                target: RepositoryTarget::Organization(org.id),
            },
            "org",
            true,
        ),
    ] {
        repos.push(
            codeyard_app::repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                owner,
                name,
                None,
                private,
                None,
            )
            .await
            .unwrap(),
        );
    }
    let mut issues = Vec::new();
    for (repo, author, body) in [
        (&repos[0], alice, "Root"),
        (&repos[1], bob, "@alice mentioned"),
        (&repos[2], alice, "Organization"),
    ] {
        issues.push(
            db::issues::create_issue(f.pool(), repo.id, author, "Issue", body)
                .await
                .unwrap(),
        );
    }
    db::repositories::access::grant_repo_access(
        f.pool(),
        alice,
        repos[1].id,
        codeyard_db_core::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    for index in [0, 2] {
        db::assignees::assign_user(
            f.pool(),
            alice,
            repos[index].id,
            db::assignees::AssignedItem::Issue(issues[index].id),
            alice,
        )
        .await
        .unwrap();
    }
    let label = db::labels::create_label(f.pool(), alice, repos[0].id, "bug", "123456", None)
        .await
        .unwrap();
    db::labels::add_issue_label(f.pool(), alice, repos[0].id, issues[0].id, label.id)
        .await
        .unwrap();
    let router = f.router(root.path()).await;
    let mut tokens_out = Vec::new();
    for id in [alice, bob] {
        let browser = Client::login(&router, f.pool(), id).await;
        let token = tokens::create(
            f.pool(),
            id,
            Some(&browser.session),
            TokenInput {
                name: "read",
                scope: "read",
                expires_at: None,
            },
        )
        .await
        .unwrap();
        tokens_out.push(format!("Bearer {}", token.value));
    }
    let token = &tokens_out[0];
    let other = &tokens_out[1];
    for (base, op) in [
        ("/api/v3/issues", "issues/list"),
        ("/api/v3/user/issues", "issues/list-for-authenticated-user"),
    ] {
        for (query, count) in [
            ("", 2),
            ("?filter=all", 3),
            ("?filter=created", 2),
            ("?filter=mentioned", 1),
            ("?filter=subscribed", 3),
            ("?filter=all&labels=bug", 1),
            ("?filter=all&owned=true", 1),
            ("?filter=all&collab=true", 1),
            ("?filter=all&orgs=true", 1),
            ("?filter=all&owned=false&collab=false&orgs=false", 0),
            ("?filter=all&since=9999-01-01T00%3A00%3A00Z", 0),
        ] {
            let got = document(
                request(
                    &router,
                    "GET",
                    &format!("{base}{query}"),
                    Some(token),
                    Value::Null,
                )
                .await,
                op,
                200,
            )
            .await;
            assert_eq!(got.as_array().unwrap().len(), count, "{base}{query}: {got}");
            for item in got.as_array().unwrap() {
                assert!(item["repository"]["full_name"].is_string());
            }
        }
        let page = request(
            &router,
            "GET",
            &format!("{base}?filter=all&per_page=1"),
            Some(token),
            Value::Null,
        )
        .await;
        assert!(page.headers()["link"].to_str().unwrap().contains("page=2"));
        let first = document(page, op, 200).await;
        let next = document(
            request(
                &router,
                "GET",
                &format!("{base}?filter=all&per_page=1&page=2"),
                Some(token),
                Value::Null,
            )
            .await,
            op,
            200,
        )
        .await;
        assert_ne!(first[0]["id"], next[0]["id"]);
        let other_items = document(
            request(
                &router,
                "GET",
                &format!("{base}?filter=all"),
                Some(other),
                Value::Null,
            )
            .await,
            op,
            200,
        )
        .await;
        assert_eq!(other_items.as_array().unwrap().len(), 1);
        assert_eq!(
            request(&router, "GET", base, None, Value::Null)
                .await
                .status(),
            axum::http::StatusCode::UNAUTHORIZED
        );
    }
    let org_path = "/api/v3/orgs/collective/issues";
    let op = "issues/list-for-org";
    let got = document(
        request(&router, "GET", org_path, Some(token), Value::Null).await,
        op,
        200,
    )
    .await;
    assert_eq!(got.as_array().unwrap().len(), 1);
    assert_eq!(got[0]["repository"]["owner"]["type"], "Organization");
    let got = document(
        request(
            &router,
            "GET",
            &format!("{org_path}?filter=all"),
            Some(other),
            Value::Null,
        )
        .await,
        op,
        200,
    )
    .await;
    assert!(got.as_array().unwrap().is_empty());
    document(
        request(
            &router,
            "GET",
            "/api/v3/orgs/absent/issues",
            Some(token),
            Value::Null,
        )
        .await,
        op,
        404,
    )
    .await;
    db::repositories::engagement::set_watch_level(f.pool(), alice, repos[0].id, "ignoring", "")
        .await
        .unwrap();
    let got = document(
        request(
            &router,
            "GET",
            "/api/v3/issues?filter=subscribed",
            Some(token),
            Value::Null,
        )
        .await,
        "issues/list",
        200,
    )
    .await;
    assert_eq!(got.as_array().unwrap().len(), 2);
    f.execute(
        "UPDATE issues SET state='closed' WHERE id=$1",
        &[issues[2].id],
    )
    .await;
    let got = document(
        request(
            &router,
            "GET",
            &format!("{org_path}?state=closed"),
            Some(token),
            Value::Null,
        )
        .await,
        op,
        200,
    )
    .await;
    assert_eq!(got.as_array().unwrap().len(), 1);
    let got = document(
        request(&router, "GET", org_path, Some(token), Value::Null).await,
        op,
        200,
    )
    .await;
    assert!(got.as_array().unwrap().is_empty());
    let pull = db::pull_requests::records::create_pull_request(
        f.pool(),
        db::pull_requests::records::NewPullRequest {
            issue: None,
            maintainer_can_modify: false,
            repo_id: repos[0].id,
            author_id: alice,
            head_repo_id: None,
            title: "Pull",
            body: "",
            head_branch: "topic",
            head_sha: &"a".repeat(40),
            base_branch: "main",
            base_sha: &"b".repeat(40),
            is_draft: false,
        },
    )
    .await
    .unwrap();
    db::assignees::assign_user(
        f.pool(),
        alice,
        repos[0].id,
        db::assignees::AssignedItem::PullRequest(pull.id),
        alice,
    )
    .await
    .unwrap();
    for (query, count) in [
        ("filter=all&state=all", 4),
        ("filter=all&state=all&pulls=false", 3),
    ] {
        let got = document(
            request(
                &router,
                "GET",
                &format!("/api/v3/issues?{query}"),
                Some(token),
                Value::Null,
            )
            .await,
            "issues/list",
            200,
        )
        .await;
        assert_eq!(got.as_array().unwrap().len(), count);
    }
    for query in [
        "filter=invalid",
        "state=invalid",
        "sort=invalid",
        "direction=invalid",
        "since=invalid",
        "owned=invalid",
    ] {
        document(
            request(
                &router,
                "GET",
                &format!("/api/v3/issues?{query}"),
                Some(token),
                Value::Null,
            )
            .await,
            "issues/list",
            422,
        )
        .await;
    }
    f.finish().await;
}
#[tokio::test]
async fn github_global_issues_sqlite() {
    lists(false).await;
}
#[tokio::test]
async fn github_global_issues_postgres() {
    lists(true).await;
}

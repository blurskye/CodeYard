use super::github::{document, request};
use crate::support::{database::Database, http_client::Client};
use codeyard::backend::{
    db,
    identity::tokens::{self, TokenInput},
};
use serde_json::{json, Value};
fn path(q: &str, extra: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", q)
        .finish();
    format!("/api/v3/search/issues?{query}{extra}")
}
async fn search(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (name, private) in [("proj", false), ("hidden", true)] {
        repos.push(
            codeyard::backend::repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                db::repositories::creation::RepositoryCreationOwner::personal(alice),
                name,
                None,
                private,
                None,
            )
            .await
            .unwrap(),
        );
    }
    let repo = &repos[0];
    let hidden = &repos[1];
    let first =
        db::issues::create_issue(f.pool(), repo.id, alice, "Fix café search", "needle @bob")
            .await
            .unwrap();
    let second = db::issues::create_issue(f.pool(), repo.id, alice, "Other title", "payload")
        .await
        .unwrap();
    let secret = db::issues::create_issue(
        f.pool(),
        hidden.id,
        alice,
        "Secret café search",
        "privateterm",
    )
    .await
    .unwrap();
    let pull = db::pull_requests::records::create_pull_request(
        f.pool(),
        db::pull_requests::records::NewPullRequest {
            issue: None,
            maintainer_can_modify: false,
            repo_id: repo.id,
            author_id: alice,
            head_repo_id: None,
            title: "Pull café search",
            body: "root",
            head_branch: "topic",
            head_sha: &"a".repeat(40),
            base_branch: "main",
            base_sha: &"b".repeat(40),
            is_draft: true,
        },
    )
    .await
    .unwrap();
    db::issues::add_issue_comment(
        f.pool(),
        second.repo_id,
        second.id,
        bob,
        "Discussion café search",
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(f.pool(), bob).await,
        ),
    )
    .await
    .unwrap();
    db::pull_requests::reviews::add_pull_request_review(
        f.pool(),
        pull.id,
        alice,
        &db::users::accounts::AccountCredential::Session(
            &crate::support::live_session(f.pool(), alice).await,
        ),
        "commented",
        Some("reviewvisible"),
        Some(&pull.head_sha),
    )
    .await
    .unwrap();
    db::pull_requests::reviews::add_pending_pull_request_comment(
        f.pool(),
        db::pull_requests::reviews::NewPullRequestComment {
            start_line: None,
            start_side: None,
            pr_id: pull.id,
            author_id: alice,
            body: "draftsecret",
            path: Some("file.txt"),
            line: Some(1),
            side: Some("RIGHT"),
            commit_sha: Some(&pull.head_sha),
            in_reply_to_comment_id: None,
            expected_revisions: Some((&pull.head_sha, &pull.base_sha)),
        },
    )
    .await
    .unwrap();
    db::assignees::assign_user(
        f.pool(),
        alice,
        repo.id,
        db::assignees::AssignedItem::Issue(first.id),
        alice,
    )
    .await
    .unwrap();
    let label = db::labels::create_label(f.pool(), alice, repo.id, "priority: bug", "123456", None)
        .await
        .unwrap();
    db::labels::add_issue_label(f.pool(), alice, repo.id, first.id, label.id)
        .await
        .unwrap();
    let router = f.router(root.path()).await;
    let mut credentials = Vec::new();
    for (id, scope) in [(alice, "write"), (alice, "read"), (bob, "read")] {
        let browser = Client::login(&router, f.pool(), id).await;
        let token = tokens::create(
            f.pool(),
            id,
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
    let (write, read, other) = (&credentials[0], &credentials[1], &credentials[2]);
    let second_path = format!("/api/v3/repos/alice/proj/issues/{}", second.number);
    document(
        request(
            &router,
            "PATCH",
            &second_path,
            Some(write),
            json!({"state":"closed"}),
        )
        .await,
        "issues/update",
        200,
    )
    .await;
    db::discussions::set_lock(
        f.pool(),
        alice,
        repo.id,
        db::assignees::AssignedItem::Issue(second.id),
        true,
        Some("resolved"),
    )
    .await
    .unwrap();
    for content in ["+1", "heart"] {
        document(
            request(
                &router,
                "POST",
                &format!("/api/v3/repos/alice/proj/issues/{}/reactions", first.number),
                Some(write),
                json!({"content":content}),
            )
            .await,
            "reactions/create-for-issue",
            201,
        )
        .await;
    }
    for (id, day) in [(first.id, "01"), (second.id, "02"), (secret.id, "04")] {
        f.execute(
            &format!("UPDATE issues SET created_at='2024-01-{day}T00:00:00.000000Z' WHERE id=$1"),
            &[id],
        )
        .await;
    }
    f.execute(
        "UPDATE pull_requests SET created_at='2024-01-03T00:00:00.000000Z' WHERE id=$1",
        &[pull.id],
    )
    .await;
    let op = "search/issues-and-pull-requests";
    for (token, count) in [
        (None, 3),
        (Some(read.as_str()), 4),
        (Some(other.as_str()), 3),
    ] {
        let got = document(
            request(&router, "GET", &path("café", ""), token, Value::Null).await,
            op,
            200,
        )
        .await;
        assert_eq!(got["total_count"], count, "{got}");
        assert_eq!(got["items"].as_array().unwrap().len(), count as usize);
        assert_eq!(got["search_type"], "lexical");
        assert_eq!(got["incomplete_results"], false);
    }
    for (q, count) in [
        ("café in:title", 2),
        ("café in:body", 0),
        ("café in:comments", 1),
        ("repo:alice/hidden café", 0),
        ("is:issue café", 2),
        ("is:pr café", 1),
        ("is:open café", 2),
        ("is:closed café", 1),
        ("is:draft", 1),
        ("draft:false café", 2),
        ("café label:\"priority: bug\"", 1),
        ("(label:\"priority: bug\" OR is:pr) -is:closed", 2),
        ("assignee:alice", 1),
        ("no:assignee café", 2),
        ("mentions:bob", 1),
        ("commenter:bob", 1),
        ("involves:bob", 2),
        ("reviewvisible", 1),
        ("draftsecret", 0),
        ("comments:>=1 café", 1),
        ("reactions:>=2", 1),
        ("interactions:>=1 café", 2),
        ("created:2024-01-01..2024-01-03", 3),
        ("created:>2024-01-01", 2),
        ("no:label café", 2),
        ("is:locked", 1),
        ("privateterm", 0),
        ("\"' OR 1=1 --\"", 0),
    ] {
        let native = db::discussion_search::search(
            f.pool(),
            Some(bob),
            &db::discussion_search::Search::parse(q).unwrap(),
            db::discussion_search::Sort::Best,
            false,
            0,
            30,
        )
        .await;
        if let Err(error) = native {
            panic!("{q}: {error:#}");
        }
        let got = document(
            request(&router, "GET", &path(q, ""), Some(other), Value::Null).await,
            op,
            200,
        )
        .await;
        assert_eq!(got["total_count"], count, "{q}: {got}");
    }
    for (q, count) in [
        ("repo:alice/proj repo:alice/hidden café", 4),
        ("café -repo:alice/proj", 1),
        ("assignee:@me", 1),
        ("mentions:@me", 0),
    ] {
        let got = document(
            request(&router, "GET", &path(q, ""), Some(read), Value::Null).await,
            op,
            200,
        )
        .await;
        assert_eq!(got["total_count"], count, "{q}: {got}");
    }
    // Pagination starts a separate conversation with its own test budget.
    f.execute("DELETE FROM github_api_limits WHERE resource='search'", &[])
        .await;
    let response = request(
        &router,
        "GET",
        &path("café", "&per_page=1&sort=created&order=asc"),
        Some(other),
        Value::Null,
    )
    .await;
    assert!(response.headers()["link"]
        .to_str()
        .unwrap()
        .contains("page=2"));
    let first_page = document(response, op, 200).await;
    assert_eq!(first_page["items"][0]["id"], first.id);
    let next = document(
        request(
            &router,
            "GET",
            &path("café", "&per_page=1&page=2&sort=created&order=asc"),
            Some(other),
            Value::Null,
        )
        .await,
        op,
        200,
    )
    .await;
    assert_eq!(next["items"][0]["id"], second.id);
    let last = request(
        &router,
        "GET",
        &path("café", "&per_page=1&page=3&sort=created&order=asc"),
        Some(other),
        Value::Null,
    )
    .await;
    assert!(!last.headers()["link"]
        .to_str()
        .unwrap()
        .contains("rel=\"next\""));
    let last = document(last, op, 200).await;
    assert_eq!(last["items"][0]["number"], pull.number);
    assert!(last["items"][0]["pull_request"].is_object());
    for sort in [
        "comments",
        "reactions",
        "reactions-+1",
        "reactions--1",
        "reactions-smile",
        "reactions-thinking_face",
        "reactions-heart",
        "reactions-tada",
        "interactions",
        "updated",
    ] {
        document(
            request(
                &router,
                "GET",
                &path(
                    "café",
                    &format!(
                        "&sort={}",
                        url::form_urlencoded::byte_serialize(sort.as_bytes()).collect::<String>()
                    ),
                ),
                Some(read),
                Value::Null,
            )
            .await,
            op,
            200,
        )
        .await;
    }
    for (q, extra) in [
        ("", ""),
        ("is:bad", ""),
        ("comments:no", ""),
        ("foo OR", ""),
        ("created:bad", ""),
        ("café", "&sort=bad"),
        ("café", "&order=bad"),
        ("café", "&per_page=100&page=11"),
    ] {
        document(
            request(&router, "GET", &path(q, extra), Some(read), Value::Null).await,
            op,
            422,
        )
        .await;
    }
    let fallback = document(
        request(
            &router,
            "GET",
            &path("café", "&search_type=hybrid"),
            Some(read),
            Value::Null,
        )
        .await,
        op,
        200,
    )
    .await;
    assert_eq!(fallback["search_type"], "lexical");
    assert_eq!(
        fallback["lexical_fallback_reason"],
        json!(["service_unavailable"])
    );
    f.finish().await;
}
#[tokio::test]
async fn github_search_sqlite() {
    search(false).await;
}
#[tokio::test]
async fn github_search_postgres() {
    search(true).await;
}

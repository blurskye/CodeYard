use super::github::exchange;
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use base64::Engine;
use codeyard::backend::db;
use codeyard::backend::identity::tokens;
use codeyard_app::repositories::store::repo_store;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn update(
    router: &Router,
    url: &str,
    token: &str,
    body: &str,
    expected: StatusCode,
) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(url)
                .header("authorization", token)
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        let value = serde_json::from_slice(&bytes).unwrap();
        assert_github_schema("pulls/update-branch", status.as_u16(), &value);
        value
    };
    assert_eq!(status, expected, "{value}");
    value
}

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_app::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let mut credentials = Vec::new();
    for (id, client, scope) in [
        (alice_id, &alice, "write"),
        (alice_id, &alice, "read"),
        (bob_id, &bob, "write"),
    ] {
        let token = tokens::create(
            f.pool(),
            id,
            Some(&client.session),
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
    let (owner, reader, visitor) = (
        credentials[0].as_str(),
        credentials[1].as_str(),
        credentials[2].as_str(),
    );
    let base = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature", &base).await;
    let head = alice.save("feature", "feature.txt", &base).await;
    let pull = alice.open_draft("Feature", "feature", "main").await;
    let number = pull["number"].as_i64().unwrap();
    let url = format!("/api/v3/repos/alice/proj/pulls/{number}/update-branch");
    let main = alice.save("main", "upstream.txt", &base).await;
    update(&router, &url, reader, "{}", StatusCode::FORBIDDEN).await;
    update(&router, &url, visitor, "{}", StatusCode::FORBIDDEN).await;
    update(
        &router,
        &url,
        owner,
        &json!({"expected_head_sha":base}).to_string(),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    // A body that is not JSON is a 400, as GitHub answers "Problems parsing JSON".
    update(&router, &url, owner, "{", StatusCode::BAD_REQUEST).await;
    // Updating a head is a direct push, not a bypass of its PR/linear-history rules.
    codeyard_db_repos::repositories::protection::upsert_branch_protection_rule(
        f.pool(),
        codeyard_db_repos::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: alice_id,
            repo_id: repo.id,
            pattern: "feature",
            require_pull_request: false,
            require_linear_history: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    update(&router, &url, owner, "{}", StatusCode::FORBIDDEN).await;
    let store = repo_store(&repo, None);
    assert_eq!(
        store.commit_from_primary("feature").await.unwrap().sha,
        head
    );
    codeyard_db_repos::repositories::protection::upsert_branch_protection_rule(
        f.pool(),
        codeyard_db_repos::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: alice_id,
            repo_id: repo.id,
            pattern: "feature",
            require_pull_request: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let body = update(
        &router,
        &url,
        owner,
        &json!({"expected_head_sha":head}).to_string(),
        StatusCode::ACCEPTED,
    )
    .await;
    assert!(body["url"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/repos/alice/proj/pulls/{number}")));
    let changed = store.commit_from_primary("feature").await.unwrap();
    assert_ne!(changed.sha, head);
    assert_eq!(changed.parents, vec![head, main.clone()]);
    let saved = codeyard_db_repos::pull_requests::records::get_pull_request(
        f.pool(),
        repo.id,
        number as i32,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(saved.head_sha, changed.sha);
    assert_eq!(saved.state, "open");
    assert!(saved.is_draft);
    assert!(!saved.merged);
    for body in ["", "null", "{}"] {
        update(&router, &url, owner, body, StatusCode::ACCEPTED).await;
    }
    assert_eq!(
        store.commit_from_primary("feature").await.unwrap().sha,
        changed.sha
    );
    // Conflicts leave both refs and the open pull untouched.
    alice.branch("conflict", &main).await;
    let conflict = alice
        .update_text("conflict", "README.md", "head conflict\n", &main)
        .await;
    let conflicted = alice.open_pull("Conflict", "conflict", "main").await;
    let main = alice
        .update_text("main", "README.md", "base conflict\n", &main)
        .await;
    update(
        &router,
        &format!(
            "/api/v3/repos/alice/proj/pulls/{}/update-branch",
            conflicted["number"]
        ),
        owner,
        "{}",
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert_eq!(
        store.commit_from_primary("conflict").await.unwrap().sha,
        conflict
    );
    assert_eq!(store.commit_from_primary("main").await.unwrap().sha, main);
    // GraphQL's REBASE replays the head's commits onto the base instead of merging it in.
    alice.branch("rebased", &main).await;
    let topic = alice.save("rebased", "rebased.txt", &main).await;
    let rebased = alice.open_pull("Rebased", "rebased", "main").await;
    let main = alice.save("main", "rebase-base.txt", &main).await;
    let graphql = super::github_team_reviewers::graphql;
    let found = graphql(
        &router,
        owner,
        "query($n:Int!){repository(owner:\"alice\",name:\"proj\"){pullRequest(number:$n){id}}}",
        json!({"n": rebased["number"]}),
    )
    .await;
    let id = found["data"]["repository"]["pullRequest"]["id"].clone();
    let moved = graphql(
        &router,
        owner,
        "mutation($id:ID!){updatePullRequestBranch(input:{pullRequestId:$id,updateMethod:REBASE}){pullRequest{headRefOid}}}",
        json!({"id": id}),
    )
    .await;
    assert!(moved["errors"].is_null(), "{moved}");
    let tip = store.commit_from_primary("rebased").await.unwrap();
    assert_ne!(tip.sha, topic);
    assert_eq!(tip.parents, vec![main.clone()]);
    assert_eq!(
        moved["data"]["updatePullRequestBranch"]["pullRequest"]["headRefOid"],
        tip.sha.as_str()
    );
    // The fork owner may update their head without write access to the upstream repository.
    let fork = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/forks",
            json!({}),
            StatusCode::CREATED,
        )
        .await;
    let fork_id = fork["repo"]["id"].as_i64().unwrap();
    let written=bob.expect("POST","/api/repos/bob/proj/contents/fork-feature.txt",json!({"content":base64::engine::general_purpose::STANDARD.encode("fork feature\n"),"message":"Fork feature","branch":"main","expected_revision":main}),StatusCode::CREATED).await;
    let fork_head = written["sha"].as_str().unwrap().to_owned();
    let from_fork=bob.expect("POST","/api/repos/alice/proj/pulls",json!({"title":"Fork feature","head_repo_id":fork_id,"head_branch":"main","base_branch":"main"}),StatusCode::CREATED).await;
    let fork_url = format!(
        "/api/v3/repos/alice/proj/pulls/{}/update-branch",
        from_fork["number"]
    );
    let main = alice.save("main", "later-upstream.txt", &main).await;
    // Base protections are not head protections; neither grants delegated fork write access.
    codeyard_db_repos::repositories::protection::upsert_branch_protection_rule(
        f.pool(),
        codeyard_db_repos::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: alice_id,
            repo_id: repo.id,
            pattern: "main",
            require_pull_request: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    update(&router, &fork_url, owner, "{}", StatusCode::FORBIDDEN).await;
    update(
        &router,
        &fork_url,
        visitor,
        &json!({"expected_head_sha":fork_head}).to_string(),
        StatusCode::ACCEPTED,
    )
    .await;
    let fork = codeyard_db_repos::repositories::records::get_repository_by_id(f.pool(), fork_id)
        .await
        .unwrap()
        .unwrap();
    let updated = repo_store(&fork, None)
        .commit_from_primary("main")
        .await
        .unwrap();
    assert_eq!(updated.parents, vec![fork_head, main.clone()]);
    assert_eq!(store.commit_from_primary("main").await.unwrap().sha, main);
    let (status, _, body) = exchange(
        &router,
        "pulls/update",
        "PATCH",
        &format!("/api/v3/repos/alice/proj/pulls/{number}"),
        Some(owner),
        json!({"state":"closed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    update(&router, &url, owner, "{}", StatusCode::UNPROCESSABLE_ENTITY).await;
    f.finish().await;
}
#[tokio::test]
async fn sqlite_github_update_branch() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_update_branch() {
    contract(true).await;
}

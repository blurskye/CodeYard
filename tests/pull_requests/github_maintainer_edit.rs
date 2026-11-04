use super::github::exchange;
use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::{http::StatusCode, Router};
use base64::Engine;
use codeyard::backend::db;
use codeyard::backend::identity::tokens;
use codeyard_app::repositories::lifecycle;
use codeyard_app::repositories::store::repo_store;
use codeyard_app::repositories::store::types::PullBranchUpdate;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};

async fn call(
    router: &Router,
    operation: &str,
    method: &str,
    url: &str,
    token: &str,
    body: Value,
    expected: StatusCode,
) -> Value {
    let (status, _, body) = exchange(router, operation, method, url, Some(token), body).await;
    assert_eq!(status, expected, "{operation}: {body}");
    body
}

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let carol_id = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let base = lifecycle::provision(
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
    let carol = Client::login(&router, f.pool(), carol_id).await;
    let mut credentials = Vec::new();
    for (id, client, scope) in [
        (alice_id, &alice, "write"),
        (bob_id, &bob, "write"),
        (carol_id, &carol, "write"),
        (alice_id, &alice, "read"),
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
    let (owner, author, stranger, reader) = (
        credentials[0].as_str(),
        credentials[1].as_str(),
        credentials[2].as_str(),
        credentials[3].as_str(),
    );
    let first = alice.save("main", "README.md", "unborn").await;
    let fork = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/forks",
            json!({}),
            StatusCode::CREATED,
        )
        .await;
    let fork_id = fork["repo"]["id"].as_i64().unwrap();
    let written = bob.expect("POST", "/api/repos/bob/proj/contents/feature.txt", json!({"content":base64::engine::general_purpose::STANDARD.encode("Feature\n"),"message":"Feature","branch":"main","expected_revision":first}), StatusCode::CREATED).await;
    let fork_sha = written["sha"].as_str().unwrap().to_owned();
    let collection = "/api/v3/repos/alice/proj/pulls";
    call(&router, "pulls/create", "POST", collection, owner, json!({"title":"Forged consent","head":"bob:main","base":"main","maintainer_can_modify":true}), StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM pull_requests WHERE repo_id=$1",
            &[base.id]
        )
        .await,
        0
    );
    let pull = call(
        &router,
        "pulls/create",
        "POST",
        collection,
        author,
        json!({"title":"Consented fork","head":"bob:main","base":"main"}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(pull["maintainer_can_modify"], true);
    let number = pull["number"].as_i64().unwrap() as i32;
    let url = format!("{collection}/{number}");
    let update = format!("{url}/update-branch");
    let native =
        codeyard_db_repos::pull_requests::records::get_pull_request(f.pool(), base.id, number)
            .await
            .unwrap()
            .unwrap();
    assert!(native.maintainer_can_modify);
    assert_eq!(native.maintainer_edit_granted_by_id, Some(bob_id));
    let fork = codeyard_db_repos::repositories::records::get_repository_by_id(f.pool(), fork_id)
        .await
        .unwrap()
        .unwrap();
    let target = repo_store(&fork, None);
    let source = repo_store(&base, None);
    let upstream = alice.save("main", "upstream.txt", &first).await;
    // Consent never grants general repository mutation, nor weakens the native metadata gate.
    alice.expect("POST", "/api/repos/bob/proj/contents/unscoped.txt", json!({"content":base64::engine::general_purpose::STANDARD.encode("Not scoped"),"message":"Unscoped","branch":"main","expected_revision":fork_sha}), StatusCode::FORBIDDEN).await;
    // The author without base write access may change ordinary fields and the consent together.
    let retitled = call(
        &router,
        "pulls/update",
        "PATCH",
        &url,
        author,
        json!({"title":"Retitled with consent","maintainer_can_modify":true}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(retitled["title"], "Retitled with consent");
    assert_eq!(retitled["maintainer_can_modify"], true);
    call(
        &router,
        "pulls/update",
        "PATCH",
        &url,
        stranger,
        json!({"maintainer_can_modify":false}),
        StatusCode::FORBIDDEN,
    )
    .await;
    call(
        &router,
        "pulls/update-branch",
        "PUT",
        &update,
        stranger,
        json!({}),
        StatusCode::FORBIDDEN,
    )
    .await;
    call(
        &router,
        "pulls/update-branch",
        "PUT",
        &update,
        reader,
        json!({}),
        StatusCode::FORBIDDEN,
    )
    .await;
    // A base owner is not the fork's administrator and cannot bypass its branch rules.
    codeyard_db_repos::repositories::protection::upsert_branch_protection_rule(
        f.pool(),
        codeyard_db_repos::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: bob_id,
            repo_id: fork_id,
            pattern: "main",
            require_pull_request: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    call(
        &router,
        "pulls/update-branch",
        "PUT",
        &update,
        owner,
        json!({}),
        StatusCode::FORBIDDEN,
    )
    .await;
    assert_eq!(
        target.commit_from_primary("main").await.unwrap().sha,
        fork_sha
    );
    codeyard_db_repos::repositories::protection::upsert_branch_protection_rule(
        f.pool(),
        codeyard_db_repos::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: bob_id,
            repo_id: fork_id,
            pattern: "main",
            require_pull_request: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    call(
        &router,
        "pulls/update-branch",
        "PUT",
        &update,
        owner,
        json!({"expected_head_sha":fork_sha}),
        StatusCode::ACCEPTED,
    )
    .await;
    let updated = target.commit_from_primary("main").await.unwrap();
    assert_eq!(updated.parents, vec![fork_sha, upstream.clone()]);
    assert_eq!(
        source.commit_from_primary("main").await.unwrap().sha,
        upstream
    );
    let upstream = alice.save("main", "later.txt", &upstream).await;
    let request = PullBranchUpdate {
        pull_request_id: native.id,
        base_branch: "main".into(),
        branch: "main".into(),
        head: updated.sha.clone(),
        base: upstream.clone(),
        rebase: false,
        name: "alice".into(),
        email: "alice@example.test".into(),
        token: None,
    };
    // Publication must re-read native consent, even when an admitted caller retains a pinned request.
    let revoked = call(
        &router,
        "pulls/update",
        "PATCH",
        &url,
        author,
        json!({"maintainer_can_modify":false}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(revoked["maintainer_can_modify"], false);
    assert!(target
        .update_pull_branch_as(f.pool(), fork_id, alice_id, (base.id, &source), &request)
        .await
        .unwrap_err()
        .is_write_denied());
    assert_eq!(
        target.commit_from_primary("main").await.unwrap().sha,
        updated.sha
    );
    call(
        &router,
        "pulls/update",
        "PATCH",
        &url,
        owner,
        json!({"maintainer_can_modify":true}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let allowed = call(
        &router,
        "pulls/update",
        "PATCH",
        &url,
        author,
        json!({"maintainer_can_modify":true}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(allowed["maintainer_can_modify"], true);
    let mut forged = request.clone();
    forged.branch = "other".into();
    assert!(target
        .update_pull_branch_as(f.pool(), fork_id, alice_id, (base.id, &source), &forged)
        .await
        .is_err());
    f.execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[bob_id])
        .await;
    assert!(target
        .update_pull_branch_as(f.pool(), fork_id, alice_id, (base.id, &source), &request)
        .await
        .unwrap_err()
        .is_write_denied());
    f.execute("UPDATE users SET is_banned=FALSE WHERE id=$1", &[bob_id])
        .await;
    call(
        &router,
        "pulls/update-branch",
        "PUT",
        &update,
        owner,
        json!({"expected_head_sha":updated.sha}),
        StatusCode::ACCEPTED,
    )
    .await;
    let current =
        codeyard_db_repos::pull_requests::records::get_pull_request(f.pool(), base.id, number)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(current.state, "open");
    assert_eq!(current.title, "Retitled with consent");
    call(
        &router,
        "pulls/get",
        "GET",
        &url,
        owner,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn sqlite_github_maintainer_edit() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_maintainer_edit() {
    contract(true).await;
}

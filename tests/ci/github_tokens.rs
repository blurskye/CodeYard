//! Run credentials through REST, smart Git and the live credential rechecks on both engines.
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};

use serde_json::{json, Value};
use std::collections::BTreeMap;
use tower::ServiceExt;

async fn blob(router: &Router, repo: &str, token: &str, expected: StatusCode) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v3/repos/alice/{repo}/git/blobs"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"content":"test"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(status, expected, "{body}");
    assert_github_schema("git/create-blob", status.as_u16(), &body);
}

async fn graphql(router: &Router, token: &str, query: &str, variables: Value) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/graphql")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"query":query,"variables":variables}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let actor = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repos_root = root.path().join("repos");
    let mut repos = Vec::new();
    for name in ["project", "other"] {
        repos.push(
            codeyard_repos_core::repositories::lifecycle::provision(
                (f.pool(), None),
                &repos_root,
                codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(actor),
                name,
                None,
                true,
                None,
            )
            .await
            .unwrap(),
        );
    }
    let repo = &repos[0];
    let sha = "a".repeat(40);
    let run = codeyard_db_actions::ci::enqueue_ci_run(
        f.pool(),
        codeyard_db_actions::ci::NewCiRun {
            repo_id: repo.id,
            triggered_by_id: actor,
            pipeline: "ci",
            event: "push",
            pull_request_id: None,
            branch: "main",
            head_sha: &sha,
            workflow_path: ".github/workflows/ci.yml",
            workflow_blobs: &json!({}),
            workflow_inputs: None,
            environment: None,
            manual: false,
        },
        &[actor],
    )
    .await
    .unwrap();
    let run = codeyard_db_actions::ci::claim_ci_run(
        f.pool(),
        "worker",
        "local",
        &[actor],
        &codeyard_ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .filter(|claim| claim.id == run.id)
    .unwrap();
    let definitions = BTreeMap::from([(".github/workflows/ci.yml".into(), b"permissions: {contents: write}\njobs:\n  writer: {}\n  reader: {permissions: {contents: read}}\n  triage: {permissions: {issues: write}}\n  merger: {permissions: {pull-requests: write, contents: read}}".to_vec())]);
    let router = f.router(&repos_root).await;
    f.execute("UPDATE ci_runs SET is_fork=true WHERE id=$1", &[run.id])
        .await;
    let mut fork_run = run.clone();
    fork_run.is_fork = true;
    let fork = codeyard_ci::tokens::issue(f.pool(), &fork_run, &definitions, true)
        .await
        .unwrap();
    blob(
        &router,
        "project",
        &fork.workflows[".github/workflows/ci.yml"]["writer"],
        StatusCode::FORBIDDEN,
    )
    .await;
    f.execute("UPDATE ci_runs SET is_fork=false WHERE id=$1", &[run.id])
        .await;
    let mut act_run = run.clone();
    act_run.engine = Some("act".into());
    let act = codeyard_ci::tokens::issue(f.pool(), &act_run, &definitions, false)
        .await
        .unwrap();
    assert!(act.act_limited);
    blob(&router, "project", &act.act, StatusCode::FORBIDDEN).await;
    let credentials = codeyard_ci::tokens::issue(f.pool(), &run, &definitions, false)
        .await
        .unwrap();
    assert!(credentials.act_limited);
    let jobs = &credentials.workflows[".github/workflows/ci.yml"];
    assert!(codeyard_db_identity::users::tokens::find_token(
        f.pool(),
        &codeyard_support::token_hash::token_hash(&act.act)
    )
    .await
    .unwrap()
    .is_none());
    blob(&router, "project", &jobs["writer"], StatusCode::CREATED).await;
    blob(&router, "project", &jobs["reader"], StatusCode::FORBIDDEN).await;
    blob(&router, "other", &jobs["writer"], StatusCode::NOT_FOUND).await;
    // A pull request grant alone cannot move a branch: a merge also needs contents write.
    let merge = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v3/repos/alice/project/pulls/1/merge")
                .header("authorization", format!("Bearer {}", jobs["merger"]))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(merge.status(), StatusCode::FORBIDDEN);
    let stored = codeyard_db_identity::users::tokens::find_token(
        f.pool(),
        &codeyard_support::token_hash::token_hash(&jobs["writer"]),
    )
    .await
    .unwrap()
    .unwrap();
    let bot = codeyard_db_identity::users::tokens::token_user(f.pool(), stored.id, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bot.username, codeyard_db_core::queries::ci_tokens::BOT);
    assert!(codeyard_db_identity::users::sessions::create_session(
        f.pool(),
        "bot-session",
        bot.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1)
    )
    .await
    .is_err());
    assert_eq!(codeyard_webhooks::payloads::user(&bot)["type"], "Bot");
    // The shared GraphQL implementation must not borrow this bot's grants from other runs.
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bot.id,
        repos[1].id,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let human = super::actions_objects::pat(&f, &router, actor, "write").await;
    let names = graphql(
        &router,
        &human,
        "{ repository(owner:\"alice\",name:\"other\") { id } }",
        json!({}),
    )
    .await;
    let foreign = names["data"]["repository"]["id"].as_str().unwrap();
    let names=graphql(&router,&jobs["writer"],"{ viewer { login } allowed:repository(owner:\"alice\",name:\"project\") { id name } denied:repository(owner:\"alice\",name:\"other\") { id name } }",json!({})).await;
    assert_eq!(
        names["data"]["viewer"]["login"],
        codeyard_db_core::queries::ci_tokens::BOT
    );
    assert_eq!(names["data"]["allowed"]["name"], "project");
    assert!(names["data"]["denied"].is_null());
    assert!(names["errors"]
        .as_array()
        .is_some_and(|errors| !errors.is_empty()));
    let target = names["data"]["allowed"]["id"].as_str().unwrap();
    let create="mutation($id:ID!) { createIssue(input:{repositoryId:$id,title:\"From a workflow\"}) { issue { id number title } } }";
    let denied = graphql(&router, &jobs["writer"], create, json!({"id":target})).await;
    assert_eq!(denied["errors"][0]["type"], "FORBIDDEN");
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM issues WHERE repo_id=$1", &[repo.id])
            .await,
        0
    );
    let created = graphql(&router, &jobs["triage"], create, json!({"id":target})).await;
    assert!(created.get("errors").is_none(), "{created}");
    assert_eq!(
        created["data"]["createIssue"]["issue"]["title"],
        "From a workflow"
    );
    let issue_id = created["data"]["createIssue"]["issue"]["id"]
        .as_str()
        .unwrap();
    for token in [&jobs["reader"], &jobs["writer"]] {
        let denied = graphql(
            &router,
            token,
            "query($id:ID!) { node(id:$id) { ... on Issue { title } } }",
            json!({"id":issue_id}),
        )
        .await;
        assert!(denied["data"]["node"].is_null());
        assert_eq!(denied["errors"][0]["type"], "FORBIDDEN");
    }
    let allowed = graphql(
        &router,
        &jobs["triage"],
        "query($id:ID!) { node(id:$id) { ... on Issue { title } } }",
        json!({"id":issue_id}),
    )
    .await;
    assert_eq!(allowed["data"]["node"]["title"], "From a workflow");
    let denied = graphql(&router, &jobs["triage"], create, json!({"id":foreign})).await;
    assert!(denied["errors"]
        .as_array()
        .is_some_and(|errors| !errors.is_empty()));
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM issues WHERE repo_id=$1",
            &[repos[1].id]
        )
        .await,
        0
    );
    let denied = graphql(
        &router,
        &jobs["triage"],
        "{ repository(owner:\"alice\",name:\"project\") { defaultBranchRef { name } } }",
        json!({}),
    )
    .await;
    assert_eq!(denied["errors"][0]["type"], "FORBIDDEN");
    for (token, service, expected) in [
        (&jobs["reader"], "git-upload-pack", StatusCode::OK),
        (&jobs["reader"], "git-receive-pack", StatusCode::FORBIDDEN),
        (&jobs["writer"], "git-receive-pack", StatusCode::OK),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/alice/project.git/info/refs?service={service}"))
                    .header(
                        "authorization",
                        format!(
                            "Basic {}",
                            STANDARD.encode(format!("x-access-token:{token}"))
                        ),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    {
        use crate::support::git_server;
        use crate::support::git_server::git;
        use crate::support::git_server::success;
        let bare = git2::Repository::open_bare(&repo.disk_path).unwrap();
        crate::support::production_ci::commit(&bare, "refs/heads/main", None, "checkout", None);
        drop(bare);
        let server = git_server::Server::start(root.path(), &f.connection_url()).await;
        let url = format!("{}/alice/project.git", server.base);
        success(git(
            root.path(),
            &["clone", &url, "checkout"],
            Some(&jobs["reader"]),
        ));
        std::fs::write(root.path().join("checkout/new.txt"), "from a workflow").unwrap();
        success(git(
            root.path(),
            &["-C", "checkout", "add", "new.txt"],
            None,
        ));
        success(git(
            root.path(),
            &["-C", "checkout", "commit", "-m", "Workflow change"],
            None,
        ));
        let refused = git(
            root.path(),
            &["-C", "checkout", "push", "origin", "HEAD:main"],
            Some(&jobs["reader"]),
        );
        assert!(!refused.status.success(), "read token must not push");
        success(git(
            root.path(),
            &["-C", "checkout", "push", "origin", "HEAD:main"],
            Some(&jobs["writer"]),
        ));
        drop(server);
    }
    codeyard_db_actions::ci_tokens::finish_job(
        f.pool(),
        run.id,
        "another-worker",
        ".github/workflows/ci.yml:writer",
    )
    .await
    .unwrap();
    assert!(codeyard_db_actions::ci_tokens::get(f.pool(), stored.id)
        .await
        .unwrap()
        .is_some());
    codeyard_db_actions::ci_tokens::finish_job(
        f.pool(),
        run.id,
        "worker",
        ".github/workflows/ci.yml:writer",
    )
    .await
    .unwrap();
    assert!(
        codeyard_db_actions::ci_tokens::get(f.pool(), stored.id)
            .await
            .unwrap()
            .is_some(),
        "job completion has a short grace"
    );
    f.execute(
        "UPDATE actions_github_tokens SET revoked_at=0 WHERE token_id=$1",
        &[stored.id],
    )
    .await;
    assert!(
        codeyard_db_identity::users::tokens::token_user(f.pool(), stored.id, false)
            .await
            .unwrap()
            .is_none()
    );
    f.execute(
        "UPDATE ci_runs SET status='success',completed_at='2000-01-01T00:00:00Z' WHERE id=$1",
        &[run.id],
    )
    .await;
    let reader = codeyard_db_identity::users::tokens::find_token(
        f.pool(),
        &codeyard_support::token_hash::token_hash(&jobs["reader"]),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        codeyard_db_identity::users::tokens::token_user(f.pool(), reader.id, false)
            .await
            .unwrap()
            .is_none()
    );
    f.execute("DELETE FROM ci_runs WHERE id=$1", &[run.id])
        .await;
    assert!(codeyard_db_identity::users::tokens::find_token(
        f.pool(),
        &codeyard_support::token_hash::token_hash(&jobs["reader"])
    )
    .await
    .unwrap()
    .is_none());
    f.finish().await;
}

#[tokio::test]
async fn sqlite_github_job_tokens() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_job_tokens() {
    contract(true).await;
}

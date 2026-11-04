//! Real pushes record rule suites for rulesets in evaluate mode without being refused.
use crate::support::database::Database;
use crate::support::git_server;
use crate::support::github_spec::assert_github_schema;

use codeyard_app::repositories::lifecycle;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::queries::rulesets::records::Owner;
use codeyard_db_core::queries::rulesets::Definition;
use serde_json::{json, Value};
use std::{path::Path, process::Output};

async fn git(root: &Path, args: &[&str], token: Option<&str>) -> Output {
    let root = root.to_owned();
    let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let token = token.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        git_server::git(
            &root,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
            token.as_deref(),
        )
    })
    .await
    .unwrap()
}
async fn get(url: &str, token: &str, operation: &str) -> Value {
    let response = reqwest::Client::new()
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_github_schema(operation, status, &body);
    body
}
async fn contract(pg: bool) {
    let mut fixture = Database::new(pg).await;
    fixture.complete_setup().await;
    let owner = fixture.actor("suite-pusher").await;
    let root = tempfile::tempdir().unwrap();
    let repos = root.path().join("repos");
    let repo = lifecycle::provision(
        (fixture.pool(), None),
        &repos,
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner),
        "evaluated",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let definition = Definition::parse(&json!({"name":"Main policy","target":"branch","enforcement":"evaluate","conditions":{"ref_name":{"include":["refs/heads/main"],"exclude":[]}},"rules":[{"type":"creation"},{"type":"non_fast_forward"}]})).unwrap();
    let OrganizationOutcome::Applied(ruleset) =
        codeyard_db_repos::rulesets::operations::create_ruleset(
            fixture.pool(),
            owner,
            Owner::Repository(repo.id),
            &definition,
        )
        .await
        .unwrap()
    else {
        panic!("the evaluate ruleset was not saved");
    };
    // Active rules are not enforced yet, so they are still refused.
    let active = definition.merge(&json!({"enforcement":"active"})).unwrap();
    assert!(codeyard_db_repos::rulesets::operations::create_ruleset(
        fixture.pool(),
        owner,
        Owner::Repository(repo.id),
        &active
    )
    .await
    .is_err());
    let token = git_server::write_token(fixture.pool(), owner).await;
    let server = git_server::Server::start_with_master_key(
        root.path(),
        &fixture.connection_url(),
        &"2a".repeat(32),
    )
    .await;
    let base = &server.base;
    let local = root.path().join("client");
    std::fs::create_dir(&local).unwrap();
    git_server::success(git(&local, &["init", "-b", "main"], None).await);
    std::fs::write(local.join("README.md"), "First\n").unwrap();
    git_server::success(git(&local, &["add", "."], None).await);
    git_server::success(git(&local, &["commit", "-m", "initial"], None).await);
    let remote = format!("{base}/suite-pusher/evaluated.git");
    git_server::success(git(&local, &["remote", "add", "origin", &remote], None).await);
    // Creating main fails the creation rule, but evaluate mode lets the push through.
    git_server::success(git(&local, &["push", "origin", "main"], Some(&token)).await);
    // A branch the ruleset does not target records nothing.
    git_server::success(git(&local, &["push", "origin", "main:topic"], Some(&token)).await);
    std::fs::write(local.join("README.md"), "Second\n").unwrap();
    git_server::success(git(&local, &["commit", "-am", "second"], None).await);
    git_server::success(git(&local, &["push", "origin", "main"], Some(&token)).await);
    git_server::success(git(&local, &["commit", "--amend", "-m", "rewritten"], None).await);
    git_server::success(git(&local, &["push", "--force", "origin", "main"], Some(&token)).await);
    let suites = format!("{base}/api/v3/repos/suite-pusher/evaluated/rulesets/rule-suites");
    let listed = get(&suites, &token, "repos/get-repo-rule-suites").await;
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 3, "{listed:?}");
    for suite in listed {
        assert_eq!(suite["ref"], "refs/heads/main");
        assert_eq!(suite["result"], "pass");
        assert_eq!(suite["actor_name"], "suite-pusher");
    }
    let results: Vec<_> = listed
        .iter()
        .map(|s| s["evaluation_result"].clone())
        .collect();
    assert_eq!(results, [json!("fail"), json!("pass"), json!("fail")]);
    assert_eq!(listed[2]["before_sha"], "0".repeat(40));
    let forced = get(
        &format!("{suites}/{}", listed[0]["id"]),
        &token,
        "repos/get-repo-rule-suite",
    )
    .await;
    let evaluations = forced["rule_evaluations"].as_array().unwrap();
    assert_eq!(evaluations.len(), 1, "{forced}");
    assert_eq!(evaluations[0]["rule_type"], "non_fast_forward");
    assert_eq!(evaluations[0]["result"], "fail");
    assert_eq!(evaluations[0]["enforcement"], "evaluate");
    assert_eq!(evaluations[0]["rule_source"]["id"], ruleset.id);
    assert_eq!(evaluations[0]["rule_source"]["type"], "Repository");
    let created = get(
        &format!("{suites}/{}", listed[2]["id"]),
        &token,
        "repos/get-repo-rule-suite",
    )
    .await;
    assert_eq!(created["rule_evaluations"][0]["rule_type"], "creation");
    let filtered = get(
        &format!("{suites}?rule_suite_result=fail"),
        &token,
        "repos/get-repo-rule-suites",
    )
    .await;
    assert_eq!(filtered, json!([]));
    drop(server);
    fixture.finish().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evaluate_rulesets_record_push_suites_sqlite() {
    contract(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evaluate_rulesets_record_push_suites_postgres() {
    contract(true).await;
}

//! The GraphQL boundary must not widen the REST job-token policy.
use super::*;
use codeyard_app::repositories;
use codeyard_model::node_id;
use codeyard_webhooks::payloads;
use std::collections::BTreeMap;

async fn contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let actor = f.actor("job-owner").await;
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for name in ["inside", "outside"] {
        repos.push(
            repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(actor),
                name,
                None,
                name == "inside",
                None,
            )
            .await
            .unwrap(),
        );
    }
    let run = codeyard_db_actions::ci::enqueue_ci_run(
        f.pool(),
        codeyard_db_actions::ci::NewCiRun {
            repo_id: repos[0].id,
            triggered_by_id: actor,
            pipeline: "ci",
            event: "push",
            pull_request_id: None,
            branch: "main",
            head_sha: &"a".repeat(40),
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
    let definitions = BTreeMap::from([(".github/workflows/ci.yml".into(),b"jobs:\n  contents:\n    permissions: {contents: read}\n  reader:\n    permissions: {issues: read}\n  writer:\n    permissions: {issues: write}\n".to_vec())]);
    let issued = codeyard_ci::tokens::issue(f.pool(), &run, &definitions, false)
        .await
        .unwrap();
    let tokens = &issued.workflows[".github/workflows/ci.yml"];
    let router = create_router(super::patient(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    )));
    let own_id = payloads::repository(&repos[0], "job-owner")["node_id"].clone();
    let other_id = payloads::repository(&repos[1], "job-owner")["node_id"].clone();
    for (index, repo) in repos.iter().enumerate() {
        let row = codeyard_db_repos::repositories::commit_comments::create_comment(
            f.pool(),
            codeyard_db_repos::repositories::commit_comments::CommentCreate {
                actor: actor.into(),
                repo_id: repo.id,
                sha: &"b".repeat(40),
                body: "Scoped commit discussion",
                path: None,
                position: None,
                line: None,
            },
        )
        .await
        .unwrap();
        let id = node_id::encode("CC", row.id);
        let (_, result) = call(
            &router,
            Some(&tokens["contents"]),
            "query($id:ID!){node(id:$id){id}}",
            json!({"id":id}),
        )
        .await;
        if index == 0 {
            assert_eq!(result["data"]["node"]["id"], id, "{result}");
        } else {
            assert_eq!(result["data"]["node"], Value::Null);
            assert!(result["errors"].is_array());
        }
        let (_, denied) = call(
            &router,
            Some(&tokens["reader"]),
            "query($id:ID!){node(id:$id){id}}",
            json!({"id":id}),
        )
        .await;
        assert!(denied["errors"].is_array());
    }
    let (_, denied) = call(
        &router,
        Some(&tokens["contents"]),
        "query($id:ID!){node(id:$id){id}}",
        json!({"id":node_id::encode("T",1)}),
    )
    .await;
    assert_eq!(
        denied["errors"][0]["message"],
        "Resource not accessible by token"
    );
    for (field, input, value) in [
        (
            "createRepository",
            "CreateRepositoryInput",
            json!({"name":"job-must-not-create","visibility":"PUBLIC","hasWikiEnabled":false}),
        ),
        (
            "updateRepository",
            "UpdateRepositoryInput",
            json!({"repositoryId":own_id,"description":"job-must-not-edit"}),
        ),
    ] {
        let document =
            format!("mutation($input:{input}!){{{field}(input:$input){{repository{{id}}}}}}");
        let (_, denied) = call(
            &router,
            Some(&tokens["writer"]),
            &document,
            json!({"input":value}),
        )
        .await;
        assert_eq!(
            denied["errors"][0]["message"], "Resource not accessible by token",
            "{denied}"
        );
    }
    let metadata = query(
        &router,
        &tokens["contents"],
        "{ viewer { login } repository(owner:\"job-owner\",name:\"inside\") { name } }",
        json!({}),
    )
    .await;
    assert_eq!(
        metadata["viewer"]["login"],
        codeyard_db_core::queries::ci_tokens::BOT
    );
    let (_, denied) = call(
        &router,
        Some(&tokens["contents"]),
        "{ repository(owner:\"job-owner\",name:\"inside\") { issues { totalCount } } }",
        json!({}),
    )
    .await;
    assert_eq!(
        denied["errors"][0]["message"],
        "Resource not accessible by token"
    );
    let (_, denied) = call(
        &router,
        Some(&tokens["writer"]),
        "{ repository(owner:\"job-owner\",name:\"outside\") { id } }",
        json!({}),
    )
    .await;
    assert_eq!(denied["errors"][0]["type"], "NOT_FOUND");
    let (_, denied) = call(
        &router,
        Some(&tokens["writer"]),
        "query($ids:[ID!]!) { nodes(ids:$ids) { id } }",
        json!({"ids":[other_id]}),
    )
    .await;
    assert_eq!(denied["data"]["nodes"], json!([null]));
    let create = "mutation($input:CreateIssueInput!) { createIssue(input:$input) { issue { id stateReason } } }";
    let (_, denied) = call(
        &router,
        Some(&tokens["reader"]),
        create,
        json!({"input":{"repositoryId":own_id,"title":"Denied"}}),
    )
    .await;
    assert!(denied["errors"].is_array(), "{denied}");
    let data = query(
        &router,
        &tokens["writer"],
        create,
        json!({"input":{"repositoryId":own_id,"title":"Job issue"}}),
    )
    .await;
    let issue = data["createIssue"]["issue"]["id"].clone();
    let (_, denied) = call(
        &router,
        Some(&tokens["contents"]),
        "query($id:ID!) { node(id:$id) { id } }",
        json!({"id":issue}),
    )
    .await;
    assert_eq!(denied["errors"][0]["type"], "FORBIDDEN");
    for reason in ["COMPLETED", "NOT_PLANNED", "DUPLICATE"] {
        let data = query(&router,&tokens["writer"],"mutation($input:CloseIssueInput!) { closeIssue(input:$input) { issue { state stateReason } } }",json!({"input":{"issueId":issue,"stateReason":reason}})).await;
        assert_eq!(data["closeIssue"]["issue"]["stateReason"], reason);
        let data = query(&router,&tokens["writer"],"mutation($input:ReopenIssueInput!) { reopenIssue(input:$input) { issue { state stateReason } } }",json!({"input":{"issueId":issue}})).await;
        assert_eq!(data["reopenIssue"]["issue"]["stateReason"], "REOPENED");
    }
}
#[tokio::test]
async fn sqlite_graphql_job_tokens() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_graphql_job_tokens() {
    contract(true).await;
}

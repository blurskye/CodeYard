use super::{call, db, json, node_id, query, request};
use axum::{http::StatusCode, Router};
pub(super) async fn exercise(
    state: &codeyard::backend::app::AppState,
    router: &Router,
    tokens: (&str, &str),
    repo: i64,
    actors: (i64, i64),
) {
    let (alice, bob) = tokens;
    db::repositories::access::change_personal_repository_collaborator(
        &state.pool,
        actors.0,
        repo,
        "graphql-bob",
        Some(db::models::AccessLevel::Read),
    )
    .await
    .unwrap();
    // Fork provisioning is fixture setup through the same domain operation as the web form.
    let fork = codeyard::backend::repositories::lifecycle::fork(
        &state.pool,
        None,
        &state.repos_path,
        &state.notification_broadcaster,
        actors.1,
        repo,
        "graphql-alice",
    )
    .await
    .unwrap();
    let path = "/api/v3/repos/graphql-bob/proj";
    let mut base = serde_json::Value::Null;
    for _ in 0..100 {
        let (status, value) = request(
            router,
            Some(bob),
            "GET",
            &format!("{path}/branches/main"),
            json!(null),
        )
        .await;
        if status == StatusCode::OK {
            base = value;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(base["commit"]["sha"].is_string(), "{base}");
    let (status, value) = request(
        router,
        Some(bob),
        "POST",
        &format!("{path}/git/refs"),
        json!({"ref":"refs/heads/maintainer-grant","sha":base["commit"]["sha"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    let (status, changed) = request(
        router,
        Some(bob),
        "PUT",
        &format!("{path}/contents/grant.txt"),
        json!({"branch":"maintainer-grant","message":"Maintainer grant","content":"Z3JhbnQK"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{changed}");
    let created=query(router,bob,"mutation($input:CreatePullRequestInput!){createPullRequest(input:$input){pullRequest{id maintainerCanModify}}}",json!({"input":{"repositoryId":node_id::encode("R",repo),"headRepositoryId":node_id::encode("R",fork.id),"title":"Maintainer consent","baseRefName":"main","headRefName":"maintainer-grant","maintainerCanModify":true}})).await;
    let pull = created["createPullRequest"]["pullRequest"]["id"].clone();
    assert_eq!(
        created["createPullRequest"]["pullRequest"]["maintainerCanModify"],
        true
    );
    let pending=query(router,alice,"mutation($input:AddPullRequestReviewInput!){addPullRequestReview(input:$input){pullRequestReview{id state}}}",json!({"input":{"pullRequestId":pull,"body":"Private draft"}})).await;
    let review = pending["addPullRequestReview"]["pullRequestReview"]["id"].clone();
    let (_, hidden) = call(
        router,
        Some(bob),
        "query($id:ID!){node(id:$id){id}}",
        json!({"id":review}),
    )
    .await;
    assert_eq!(hidden["data"]["node"], serde_json::Value::Null, "{hidden}");
    assert!(hidden["errors"].is_array());
    let visible = query(
        router,
        bob,
        "query($id:ID!){node(id:$id){...on PullRequest{reviews(first:10){totalCount nodes{id}}}}}",
        json!({"id":pull}),
    )
    .await;
    assert_eq!(visible["node"]["reviews"]["totalCount"], 0);
    query(router,alice,"mutation($input:DeletePullRequestReviewInput!){deletePullRequestReview(input:$input){clientMutationId}}",json!({"input":{"pullRequestReviewId":review}})).await;
    let view = query(
        router,
        alice,
        "query($id:ID!){node(id:$id){...on PullRequest{headRepository{id} maintainerCanModify}}}",
        json!({"id":pull}),
    )
    .await;
    assert_eq!(view["node"]["headRepository"], serde_json::Value::Null);
    let updated=query(router,alice,"mutation($input:UpdatePullRequestBranchInput!){updatePullRequestBranch(input:$input){pullRequest{headRefOid}}}",json!({"input":{"pullRequestId":pull,"expectedHeadOid":changed["commit"]["sha"]}})).await;
    let head = updated["updatePullRequestBranch"]["pullRequest"]["headRefOid"].clone();
    assert!(head.is_string());
    // A pull-specific grant never becomes general write access to the fork.
    let (status, _) = request(
        router,
        Some(alice),
        "PATCH",
        &format!("{path}/git/refs/heads/main"),
        json!({"sha":head}),
    )
    .await;
    assert!(!status.is_success());
    let revoked=query(router,bob,"mutation($input:UpdatePullRequestInput!){updatePullRequest(input:$input){pullRequest{maintainerCanModify}}}",json!({"input":{"pullRequestId":pull,"maintainerCanModify":false}})).await;
    assert_eq!(
        revoked["updatePullRequest"]["pullRequest"]["maintainerCanModify"],
        false
    );
    let(_,denied)=call(router,Some(alice),"mutation($input:UpdatePullRequestBranchInput!){updatePullRequestBranch(input:$input){pullRequest{id}}}",json!({"input":{"pullRequestId":pull,"expectedHeadOid":head}})).await;
    assert!(denied["errors"].is_array(), "{denied}");
    let(_,denied)=call(router,Some(alice),"mutation($input:UpdatePullRequestInput!){updatePullRequest(input:$input){pullRequest{id}}}",json!({"input":{"pullRequestId":pull,"maintainerCanModify":true}})).await;
    assert!(denied["errors"].is_array(), "{denied}");
}

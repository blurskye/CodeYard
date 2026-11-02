//! Requested teams are durable native identities, with live nested membership and visibility.
use super::github::exchange;
use crate::support::{database::Database, http_client::Client, organizations::applied};
use axum::http::StatusCode;
use base64::Engine;
use codeyard::backend::{
    db::{self, organizations as orgs},
    identity::tokens::{self, TokenInput},
};
use serde_json::{json, Value};
use tower::ServiceExt;

pub(super) async fn graphql(
    router: &axum::Router,
    auth: &str,
    query: &str,
    variables: Value,
) -> Value {
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/graphql")
                .header("authorization", auth)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"query":query,"variables":variables}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let eve = f.actor("eve").await;
    let org = applied(
        orgs::create_organization(
            f.pool(),
            alice,
            &orgs::NewOrganization::new(
                "review-org",
                orgs::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        orgs::add_organization_member(f.pool(), alice, org.id, bob, db::models::OrgRole::Member)
            .await
            .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner {
            actor_id: alice,
            target: db::repositories::creation::RepositoryTarget::Organization(org.id),
        },
        "proj",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        f.pool(),
        eve,
        repo.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let owner = Client::login(&router, f.pool(), alice).await;
    let reviewer = Client::login(&router, f.pool(), bob).await;
    let outsider = Client::login(&router, f.pool(), eve).await;
    let mut auth = Vec::new();
    for (user, client, scope) in [
        (alice, &owner, "write"),
        (bob, &reviewer, "write"),
        (eve, &outsider, "read"),
        (alice, &owner, "read"),
    ] {
        let token = tokens::create(
            f.pool(),
            user,
            Some(&client.session),
            TokenInput {
                name: "team reviews",
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        auth.push(format!("Bearer {}", token.value));
    }
    let (_, _, parent) = exchange(
        &router,
        "teams/create",
        "POST",
        "/api/v3/orgs/review-org/teams",
        Some(&auth[0]),
        json!({"name":"owners","privacy":"closed"}),
    )
    .await;
    let parent_id = parent["id"].as_i64().unwrap();
    let (status, _, _) = exchange(
        &router,
        "teams/create",
        "POST",
        "/api/v3/orgs/review-org/teams",
        Some(&auth[0]),
        json!({"name":"nested","parent_team_id":parent_id}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = exchange(
        &router,
        "teams/add-or-update-membership-for-user-in-org",
        "PUT",
        "/api/v3/orgs/review-org/teams/nested/memberships/bob",
        Some(&auth[0]),
        json!({"role":"member"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = exchange(
        &router,
        "teams/add-or-update-repo-permissions-in-org",
        "PUT",
        "/api/v3/orgs/review-org/teams/owners/repos/review-org/proj",
        Some(&auth[0]),
        json!({"permission":"push"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let api = "/api/repos/review-org/proj";
    let base = owner.expect("POST", &format!("{api}/contents/CODEOWNERS"), json!({"branch":"main","expected_revision":"unborn","message":"Owners","content":base64::engine::general_purpose::STANDARD.encode("* @review-org/owners\n")}), StatusCode::CREATED).await;
    owner
        .expect(
            "POST",
            &format!("{api}/branches"),
            json!({"name":"feature","sha":base["sha"]}),
            StatusCode::OK,
        )
        .await;
    let head = owner.expect("POST", &format!("{api}/contents/a.txt"), json!({"branch":"feature","expected_revision":base["sha"],"message":"Change","content":base64::engine::general_purpose::STANDARD.encode("one\ntwo\n")}), StatusCode::CREATED).await;
    let pull = owner
        .expect(
            "POST",
            &format!("{api}/pulls"),
            json!({"title":"Team review","head_branch":"feature","base_branch":"main"}),
            StatusCode::CREATED,
        )
        .await;
    let number = pull["number"].as_i64().unwrap();
    let path = format!("/api/v3/repos/review-org/proj/pulls/{number}");
    let (_, _, requested) = exchange(
        &router,
        "pulls/list-requested-reviewers",
        "GET",
        &format!("{path}/requested_reviewers"),
        Some(&auth[0]),
        Value::Null,
    )
    .await;
    assert_eq!(requested["teams"][0]["id"], parent_id);
    assert!(
        requested["users"].as_array().unwrap().is_empty(),
        "CODEOWNERS must request the team, not snapshot each member"
    );
    let query = "query($number:Int!){repository(owner:\"review-org\",name:\"proj\"){pullRequest(number:$number){id reviewRequests(first:1){totalCount pageInfo{hasNextPage} nodes{id databaseId asCodeOwner requestedReviewer{__typename ... on Team{id slug name organization{login}}}}}}}}";
    let data = graphql(&router, &auth[0], query, json!({"number":number})).await;
    assert!(data.get("errors").is_none(), "{data}");
    let gql_pull = &data["data"]["repository"]["pullRequest"];
    let pull_node = gql_pull["id"].as_str().unwrap().to_owned();
    let requests = &gql_pull["reviewRequests"];
    assert_eq!(requests["totalCount"], 1);
    assert_eq!(requests["nodes"][0]["asCodeOwner"], true);
    assert_eq!(requests["nodes"][0]["requestedReviewer"]["slug"], "owners");
    assert_eq!(
        requests["nodes"][0]["requestedReviewer"]["organization"]["login"],
        "review-org"
    );
    let request_node = requests["nodes"][0]["id"].as_str().unwrap().to_owned();
    let node = graphql(&router,&auth[0],"query($id:ID!){node(id:$id){__typename ... on ReviewRequest{asCodeOwner requestedReviewer{... on Team{slug}}}}}",json!({"id":request_node})).await;
    assert!(node.get("errors").is_none(), "{node}");
    assert_eq!(node["data"]["node"]["__typename"], "ReviewRequest");
    let (_, html) = owner
        .page(&format!("/review-org/proj/pulls/{number}"))
        .await;
    assert!(html.contains("/orgs/review-org/teams/owners"), "{html}");
    let (_, dashboard) = reviewer
        .page("/pulls?tab=review-requested&state=open")
        .await;
    assert!(
        dashboard.contains(&format!("/review-org/proj/pulls/{number}")),
        "{dashboard}"
    );
    let (status, _, _) = exchange(
        &router,
        "pulls/remove-requested-reviewers",
        "DELETE",
        &format!("{path}/requested_reviewers"),
        Some(&auth[0]),
        json!({"team_reviewers":["owners"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = exchange(
        &router,
        "pulls/request-reviewers",
        "POST",
        &format!("{path}/requested_reviewers"),
        Some(&auth[3]),
        json!({"team_reviewers":["owners"]}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = exchange(
        &router,
        "pulls/request-reviewers",
        "POST",
        &format!("{path}/requested_reviewers"),
        Some(&auth[0]),
        json!({"team_reviewers":["owners","missing"]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, _, requested) = exchange(
        &router,
        "pulls/list-requested-reviewers",
        "GET",
        &format!("{path}/requested_reviewers"),
        Some(&auth[0]),
        Value::Null,
    )
    .await;
    assert!(requested["teams"].as_array().unwrap().is_empty());
    let (status, _, requested) = exchange(
        &router,
        "pulls/request-reviewers",
        "POST",
        &format!("{path}/requested_reviewers"),
        Some(&auth[0]),
        json!({"team_reviewers":["owners"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(requested["requested_teams"][0]["id"], parent_id);
    let (status, _, draft) = exchange(
        &router,
        "pulls/create-review",
        "POST",
        &format!("{path}/reviews"),
        Some(&auth[1]),
        json!({"body":"Private team draft"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    let (_, _, requested) = exchange(
        &router,
        "pulls/list-requested-reviewers",
        "GET",
        &format!("{path}/requested_reviewers"),
        Some(&auth[0]),
        Value::Null,
    )
    .await;
    assert_eq!(
        requested["teams"][0]["id"], parent_id,
        "A pending review must not consume a team request"
    );
    let (_, html) = owner
        .page(&format!("/review-org/proj/pulls/{number}"))
        .await;
    assert!(!html.contains("Private team draft"));
    let mutation = "mutation($input:RerequestReviewsInput!){rerequestReviews(input:$input){clientMutationId actor{login} pullRequest{reviewRequests(first:10){totalCount nodes{asCodeOwner requestedReviewer{... on Team{slug}}}}}}}";
    let rerequest = graphql(&router,&auth[0],mutation,json!({"input":{"pullRequestId":pull_node,"teamIds":[parent["node_id"]],"clientMutationId":"team-again"}})).await;
    assert!(rerequest.get("errors").is_none(), "{rerequest}");
    assert_eq!(
        rerequest["data"]["rerequestReviews"]["actor"]["login"],
        "alice"
    );
    assert_eq!(
        rerequest["data"]["rerequestReviews"]["clientMutationId"],
        "team-again"
    );
    assert_eq!(
        rerequest["data"]["rerequestReviews"]["pullRequest"]["reviewRequests"]["totalCount"],
        1
    );
    reviewer
        .expect(
            "POST",
            &format!("{api}/pulls/{number}/reviews"),
            json!({"state":"approved","body":"Approved by nested member","commit_sha":head["sha"]}),
            StatusCode::CREATED,
        )
        .await;
    let (_, dashboard) = reviewer
        .page("/pulls?tab=review-requested&state=open")
        .await;
    assert!(
        !dashboard.contains(&format!("/review-org/proj/pulls/{number}")),
        "{dashboard}"
    );
    let (_, _, requested) = exchange(
        &router,
        "pulls/list-requested-reviewers",
        "GET",
        &format!("{path}/requested_reviewers"),
        Some(&auth[0]),
        Value::Null,
    )
    .await;
    assert!(requested["teams"].as_array().unwrap().is_empty());
    let form = owner
        .form_reply(
            &format!("/review-org/proj/pulls/{number}/requested_reviewers"),
            &[("username", "@review-org/owners")],
        )
        .await;
    assert_eq!(form.status, StatusCode::SEE_OTHER, "{}", form.text());
    let (_, _, requested) = exchange(
        &router,
        "pulls/list-requested-reviewers",
        "GET",
        &format!("{path}/requested_reviewers"),
        Some(&auth[0]),
        Value::Null,
    )
    .await;
    assert_eq!(requested["teams"][0]["id"], parent_id);
    let removed = owner
        .form_reply(
            &format!(
                "/review-org/proj/pulls/{number}/requested_reviewers/%40review-org%2Fowners/remove"
            ),
            &[],
        )
        .await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER, "{}", removed.text());
    let (_, _, requested) = exchange(
        &router,
        "pulls/list-requested-reviewers",
        "GET",
        &format!("{path}/requested_reviewers"),
        Some(&auth[0]),
        Value::Null,
    )
    .await;
    assert!(requested["teams"].as_array().unwrap().is_empty());
    let form = owner
        .form_reply(
            &format!("/review-org/proj/pulls/{number}/requested_reviewers"),
            &[("username", "@review-org/owners")],
        )
        .await;
    assert_eq!(form.status, StatusCode::SEE_OTHER, "{}", form.text());
    let (status, _, secret) = exchange(
        &router,
        "teams/create",
        "POST",
        "/api/v3/orgs/review-org/teams",
        Some(&auth[0]),
        json!({"name":"private-reviewers","privacy":"secret"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = exchange(
        &router,
        "teams/add-or-update-repo-permissions-in-org",
        "PUT",
        "/api/v3/orgs/review-org/teams/private-reviewers/repos/review-org/proj",
        Some(&auth[0]),
        json!({"permission":"push"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = exchange(
        &router,
        "pulls/request-reviewers",
        "POST",
        &format!("{path}/requested_reviewers"),
        Some(&auth[0]),
        json!({"team_reviewers":["private-reviewers"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, headers, requested) = exchange(
        &router,
        "pulls/list-requested-reviewers",
        "GET",
        &format!("{path}/requested_reviewers?per_page=1"),
        Some(&auth[2]),
        Value::Null,
    )
    .await;
    assert!(requested["teams"]
        .as_array()
        .unwrap()
        .iter()
        .all(|team| team["id"] != secret["id"]));
    assert!(
        !headers.contains_key("link"),
        "Hidden teams must not affect pagination"
    );
    let (_, headers, _) = exchange(
        &router,
        "pulls/list-requested-reviewers",
        "GET",
        &format!("{path}/requested_reviewers?per_page=1"),
        Some(&auth[0]),
        Value::Null,
    )
    .await;
    assert!(headers.contains_key("link"));
    let (_, _, page) = exchange(
        &router,
        "pulls/list-requested-reviewers",
        "GET",
        &format!("{path}/requested_reviewers?per_page=1&page=2"),
        Some(&auth[0]),
        Value::Null,
    )
    .await;
    assert_eq!(page["teams"][0]["id"], secret["id"]);
    let visible = graphql(&router, &auth[2], query, json!({"number":number})).await;
    assert!(visible.get("errors").is_none(), "{visible}");
    assert_eq!(
        visible["data"]["repository"]["pullRequest"]["reviewRequests"]["totalCount"],
        1
    );
    assert!(!visible.to_string().contains("private-reviewers"));
    let mutation = "mutation($input:RequestReviewsInput!){requestReviews(input:$input){pullRequest{reviewRequests(first:10){totalCount nodes{requestedReviewer{... on Team{slug}}}}}}}";
    let refused = graphql(
        &router,
        &auth[3],
        mutation,
        json!({"input":{"pullRequestId":pull_node,"teamIds":[parent["node_id"]]}}),
    )
    .await;
    assert!(refused.get("errors").is_some(), "{refused}");
    let replaced = graphql(
        &router,
        &auth[0],
        mutation,
        json!({"input":{"pullRequestId":pull_node,"teamIds":[parent["node_id"]]}}),
    )
    .await;
    assert!(replaced.get("errors").is_none(), "{replaced}");
    assert_eq!(
        replaced["data"]["requestReviews"]["pullRequest"]["reviewRequests"]["totalCount"],
        1
    );
    // An eligible team with no members is still a real CODEOWNER, not a bypass.
    let (status, _, _) = exchange(
        &router,
        "teams/remove-membership-for-user-in-org",
        "DELETE",
        "/api/v3/orgs/review-org/teams/nested/memberships/bob",
        Some(&auth[0]),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    owner
        .expect(
            "POST",
            &format!("{api}/branches"),
            json!({"name":"empty-team","sha":base["sha"]}),
            StatusCode::OK,
        )
        .await;
    let fresh = owner.expect("POST", &format!("{api}/contents/b.txt"), json!({"branch":"empty-team","expected_revision":base["sha"],"message":"Empty team still owns this","content":base64::engine::general_purpose::STANDARD.encode("owned\n")}), StatusCode::CREATED).await;
    let empty = owner
        .expect(
            "POST",
            &format!("{api}/pulls"),
            json!({"title":"Empty team","head_branch":"empty-team","base_branch":"main"}),
            StatusCode::CREATED,
        )
        .await;
    let empty_number = empty["number"].as_i64().unwrap();
    let empty_path = format!("/api/v3/repos/review-org/proj/pulls/{empty_number}");
    let (_, _, requested) = exchange(
        &router,
        "pulls/list-requested-reviewers",
        "GET",
        &format!("{empty_path}/requested_reviewers"),
        Some(&auth[0]),
        Value::Null,
    )
    .await;
    assert_eq!(requested["teams"][0]["id"], parent_id);
    owner.expect("PUT", &format!("{api}/branch-protection"), json!({"pattern":"main","enforce_admins":true,"require_pull_request":true,"require_code_owner_reviews":true,"required_approving_review_count":0}), StatusCode::OK).await;
    let (status, _, refused) = exchange(
        &router,
        "pulls/merge",
        "PUT",
        &format!("{empty_path}/merge"),
        Some(&auth[0]),
        json!({"sha":fresh["sha"]}),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{refused}");
    assert!(
        refused["message"].as_str().unwrap().contains("owners"),
        "{refused}"
    );
    let (status, _, _) = exchange(
        &router,
        "teams/add-or-update-membership-for-user-in-org",
        "PUT",
        "/api/v3/orgs/review-org/teams/nested/memberships/bob",
        Some(&auth[0]),
        json!({"role":"member"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    reviewer.expect("POST", &format!("{api}/pulls/{empty_number}/reviews"), json!({"state":"approved","body":"Current nested member approval","commit_sha":fresh["sha"]}), StatusCode::CREATED).await;
    let (status, _, merged) = exchange(
        &router,
        "pulls/merge",
        "PUT",
        &format!("{empty_path}/merge"),
        Some(&auth[0]),
        json!({"sha":fresh["sha"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert_eq!(merged["merged"], true);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_github_team_reviewers() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_team_reviewers() {
    contract(true).await;
}

//! The GitHub schema and resolvers through the real token-authenticated router.
mod discussion;
mod gh_cli;
mod gh_issues;
mod gh_teams;
mod git_lane_nodes;
mod job_tokens;
mod maintainers;
mod metadata_mutations;
mod node_id;
mod objects;
mod projects;
mod repo_metadata;
mod repository_mutations;
mod reviews;
#[path = "graphql_status.rs"]
mod status;

use crate::support::{database::Database, http_client::Client, organizations::applied};
use async_graphql_parser::{
    parse_schema,
    types::{TypeKind, TypeSystemDefinition},
};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db,
    identity::tokens::{self, TokenInput},
    storage::ObjectStore,
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc};
use tower::ServiceExt;

async fn call(
    router: &Router,
    token: Option<&str>,
    query: &str,
    variables: Value,
) -> (StatusCode, Value) {
    request(
        router,
        token,
        "POST",
        "/api/graphql",
        json!({"query":query,"variables":variables}),
    )
    .await
}

async fn request(
    router: &Router,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| panic!("{status}: {}", String::from_utf8_lossy(&bytes)))
        },
    )
}

async fn query(router: &Router, token: &str, query: &str, variables: Value) -> Value {
    let diagnostic = format!("{query}\n{variables}");
    let (status, body) = call(router, Some(token), query, variables).await;
    assert_eq!(status, StatusCode::OK, "{diagnostic}\n{body}");
    assert!(body.get("errors").is_none(), "{diagnostic}\n{body}");
    body["data"].clone()
}

async fn contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("graphql-alice").await;
    let bob = f.actor("graphql-bob").await;
    f.execute(
        "UPDATE users SET email='alice@example.test' WHERE id=$1",
        &[alice],
    )
    .await;
    f.execute(
        "UPDATE users SET email='bob@example.test' WHERE id=$1",
        &[bob],
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        Some("GraphQL repository"),
        true,
        None,
    )
    .await
    .unwrap();
    let org = applied(
        db::organizations::profiles::create_organization(
            f.pool(),
            alice,
            &db::organizations::profiles::NewOrganization::new(
                "graphql-org",
                db::organizations::profiles::OrganizationProfile::new(
                    Some("GraphQL organization"),
                    None,
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let label = db::labels::create_label(f.pool(), alice, repo.id, "bug", "ff0000", Some("A bug"))
        .await
        .unwrap();
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    let router = create_router(state.clone());
    let mut credentials = Vec::new();
    let mut token_ids = Vec::new();
    for (actor, scope) in [(alice, "write"), (bob, "write"), (alice, "read")] {
        let session = Client::login(&router, f.pool(), actor).await;
        let issued = tokens::create(
            f.pool(),
            actor,
            Some(&session.session),
            TokenInput {
                name: "graphql",
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        token_ids.push(issued.token.id);
        credentials.push(issued.value);
    }
    let token = &credentials[0];
    let (status, saved) = request(
        &router,
        Some(token),
        "PUT",
        "/api/v3/repos/graphql-alice/proj/contents/README.md",
        json!({"message":"Initial commit","content":"aGVsbG8K"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let tip = saved["commit"]["sha"].clone();
    let (status, branch) = request(
        &router,
        Some(token),
        "POST",
        "/api/v3/repos/graphql-alice/proj/git/refs",
        json!({"ref":"refs/heads/feature","sha":tip}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let pull = db::pull_requests::records::create_pull_request(
        f.pool(),
        db::pull_requests::records::NewPullRequest {
            issue: None,
            maintainer_can_modify: false,
            repo_id: repo.id,
            head_repo_id: Some(repo.id),
            author_id: alice,
            title: "Pull title",
            body: "Pull body",
            head_branch: "feature",
            head_sha: tip.as_str().unwrap(),
            base_branch: "main",
            base_sha: tip.as_str().unwrap(),
            is_draft: true,
        },
    )
    .await
    .unwrap();
    let (_, rest_repo) = request(
        &router,
        Some(token),
        "GET",
        "/api/v3/repos/graphql-alice/proj",
        Value::Null,
    )
    .await;
    let (_, rest_user) = request(&router, Some(token), "GET", "/api/v3/user", Value::Null).await;
    let info = query(
        &router,
        token,
        include_str!("graphql/gh-repository-info.graphql"),
        json!({"owner":"graphql-alice","name":"proj"}),
    )
    .await;
    assert_eq!(info["repository"]["id"], rest_repo["node_id"]);
    assert_eq!(info["repository"]["viewerPermission"], "ADMIN");
    assert_eq!(info["repository"]["hasIssuesEnabled"], true);
    let listed = query(
        &router,
        token,
        include_str!("graphql/gh-repository-list.graphql"),
        json!({"owner":"graphql-alice","perPage":1,"privacy":"PRIVATE"}),
    )
    .await;
    assert_eq!(listed["repositoryOwner"]["login"], "graphql-alice");
    assert_eq!(listed["repositoryOwner"]["repositories"]["totalCount"], 1);
    assert_eq!(
        listed["repositoryOwner"]["repositories"]["nodes"][0]["nameWithOwner"],
        "graphql-alice/proj"
    );
    let listed = query(
        &router,
        &credentials[1],
        include_str!("graphql/gh-repository-list.graphql"),
        json!({"owner":"graphql-alice","perPage":1}),
    )
    .await;
    assert_eq!(listed["repositoryOwner"]["repositories"]["totalCount"], 0);
    let counts = query(&router, token, "{ repository(owner:\"graphql-alice\",name:\"proj\") { issues(states:OPEN) { totalCount } pullRequests(states:OPEN) { totalCount } watchers { totalCount } } }", json!({})).await;
    assert_eq!(counts["repository"]["pullRequests"]["totalCount"], 1);
    let (status, body) = call(&router, None, "{ viewer { login } }", json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body,
        json!({"message":"This endpoint requires you to be authenticated.","documentation_url":"https://docs.github.com/graphql/guides/forming-calls-with-graphql#authenticating-with-graphql"})
    );
    let data = query(&router,token,"{ __schema { types { name } } __type(name:\"Repository\") { fields(includeDeprecated:true) { name } } }",json!({})).await;
    let sdl = parse_schema(include_str!("../../spec/github/schema.docs.graphql")).unwrap();
    let mut names: BTreeSet<String> = ["String", "Int", "Float", "Boolean", "ID"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut fields = BTreeSet::new();
    for definition in sdl.definitions {
        if let TypeSystemDefinition::Type(ty) = definition {
            names.insert(ty.node.name.node.to_string());
            if ty.node.name.node == "Repository" {
                if let TypeKind::Object(object) = ty.node.kind {
                    fields = object
                        .fields
                        .iter()
                        .map(|f| f.node.name.node.to_string())
                        .collect();
                }
            }
        }
    }
    let actual: BTreeSet<String> = data["__schema"]["types"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .filter(|name| !name.starts_with("__"))
        .map(str::to_owned)
        .collect();
    assert_eq!(names, actual);
    let actual: BTreeSet<String> = data["__type"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(fields, actual);
    let orphan = query(
        &router,
        token,
        "{ __type(name:\"OrganizationOrUser\") { kind possibleTypes { name } } }",
        json!({}),
    )
    .await;
    assert_eq!(orphan["__type"]["kind"], "UNION");
    let members: BTreeSet<_> = orphan["__type"]["possibleTypes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| member["name"].as_str().unwrap())
        .collect();
    assert_eq!(members, BTreeSet::from(["Organization", "User"]));
    let data = query(&router,token,"{ viewer { id databaseId login name email url avatarUrl createdAt } user(login:\"graphql-bob\") { login email } organization(login:\"graphql-org\") { id databaseId login name email url avatarUrl createdAt } rateLimit { limit cost remaining resetAt used nodeCount } }",json!({})).await;
    assert_eq!(data["viewer"]["databaseId"], alice);
    // User.email is the public profile email, as on GitHub; both addresses here are private.
    assert_eq!(data["viewer"]["email"], "");
    assert_eq!(data["user"]["email"], "");
    assert_eq!(data["organization"]["databaseId"], org.id);
    for account in ["viewer", "organization"] {
        assert!(url::Url::parse(data[account]["url"].as_str().unwrap()).is_ok());
        assert!(url::Url::parse(data[account]["avatarUrl"].as_str().unwrap()).is_ok());
    }
    let viewer_id = data["viewer"]["id"].clone();
    assert_eq!(viewer_id, rest_user["node_id"]);
    assert_eq!(data["rateLimit"]["limit"], 5000);
    assert_eq!(data["rateLimit"]["cost"], 1);
    assert_eq!(
        data["rateLimit"]["remaining"].as_i64().unwrap()
            + data["rateLimit"]["used"].as_i64().unwrap(),
        5000
    );
    let data = query(&router,token,"{ repository(owner:\"graphql-alice\",name:\"proj\") { id databaseId name nameWithOwner owner { login } description url sshUrl isPrivate isFork isArchived visibility createdAt updatedAt pushedAt defaultBranchRef { name target { oid } } stargazerCount forkCount labels(first:10) { totalCount nodes { id name color description } edges { cursor node { name } } pageInfo { hasNextPage hasPreviousPage startCursor endCursor } } pullRequest(number:1) { id number title body state url author { login } createdAt closedAt baseRefName headRefName isDraft merged mergedAt labels(first:10) { totalCount } comments(first:10) { totalCount nodes { body } } } pullRequests(first:10) { totalCount nodes { number } } } }",json!({})).await;
    let repository_id = data["repository"]["id"].clone();
    assert_eq!(repository_id, rest_repo["node_id"]);
    assert_eq!(data["repository"]["defaultBranchRef"]["name"], "main");
    assert_eq!(data["repository"]["defaultBranchRef"]["target"]["oid"], tip);
    assert!(data["repository"]["pushedAt"].is_string());
    assert_eq!(data["repository"]["databaseId"], repo.id);
    assert!(url::Url::parse(data["repository"]["url"].as_str().unwrap()).is_ok());
    assert!(url::Url::parse(data["repository"]["pullRequest"]["url"].as_str().unwrap()).is_ok());
    assert_eq!(data["repository"]["nameWithOwner"], "graphql-alice/proj");
    assert_eq!(data["repository"]["labels"]["nodes"][0]["name"], label.name);
    assert_eq!(data["repository"]["pullRequest"]["number"], pull.number);
    assert_eq!(data["repository"]["pullRequests"]["totalCount"], 1);
    let pull_id = data["repository"]["pullRequest"]["id"].clone();
    let data = query(&router,token,"mutation($input:CreateIssueInput!) { createIssue(input:$input) { clientMutationId issue { id number title body state url author { login } createdAt closedAt labels(first:10) { totalCount } comments(first:10) { totalCount } } } }",json!({"input":{"repositoryId":repository_id,"title":"Issue title","body":"Issue body","clientMutationId":"one"}})).await;
    assert_eq!(data["createIssue"]["clientMutationId"], "one");
    assert_eq!(data["createIssue"]["issue"]["state"], "OPEN");
    assert!(url::Url::parse(data["createIssue"]["issue"]["url"].as_str().unwrap()).is_ok());
    let issue_id = data["createIssue"]["issue"]["id"].clone();
    let issue_number = data["createIssue"]["issue"]["number"].clone();
    for subject in [&issue_id, &pull_id] {
        let data = query(&router,token,"mutation($input:AddCommentInput!) { addComment(input:$input) { subject { id } commentEdge { node { id body author { login } createdAt } } } }",json!({"input":{"subjectId":subject,"body":"A comment"}})).await;
        assert_eq!(
            data["addComment"]["commentEdge"]["node"]["body"],
            "A comment"
        );
    }
    let stored = db::issues::get_issue(f.pool(), repo.id, issue_number.as_i64().unwrap() as i32)
        .await
        .unwrap()
        .unwrap();
    db::assignees::assign_user(
        f.pool(),
        alice,
        repo.id,
        db::assignees::AssignedItem::Issue(stored.id),
        alice,
    )
    .await
    .unwrap();
    db::reactions::create_reaction(
        f.pool(),
        alice,
        &db::users::accounts::AccountCredential::Token(token_ids[0], &tokens::token_hash(token)),
        db::reactions::Target {
            repo_id: repo.id,
            item_id: stored.id,
            subject: db::reactions::Subject::Issue,
            subject_id: stored.id,
        },
        "+1",
    )
    .await
    .unwrap();
    let status = query(
        &router,
        token,
        include_str!("graphql/gh-issue-status.graphql"),
        json!({"owner":"graphql-alice","repo":"proj","viewer":"graphql-alice"}),
    )
    .await;
    assert_eq!(status["repository"]["assigned"]["totalCount"], 1);
    assert_eq!(status["repository"]["authored"]["totalCount"], 1);
    assert_eq!(status["repository"]["mentioned"]["totalCount"], 0);
    let found = query(&router,token,include_str!("graphql/gh-pull-request-search.graphql"),json!({"q":"repo:graphql-alice/proj is:pr base:main sort:created-asc","type":"ISSUE","limit":1})).await;
    assert_eq!(found["search"]["issueCount"], 1);
    assert_eq!(found["search"]["nodes"][0]["id"], pull_id);
    let hidden = query(
        &router,
        &credentials[1],
        include_str!("graphql/gh-pull-request-search.graphql"),
        json!({"q":"repo:graphql-alice/proj is:pr","type":"ISSUE","limit":1}),
    )
    .await;
    assert_eq!(hidden["search"]["issueCount"], 0);
    let page = query(&router,token,include_str!("graphql/gh-pull-request-search.graphql"),json!({"q":"repo:graphql-alice/proj is:pr","type":"ISSUE","limit":1,"endCursor":found["search"]["pageInfo"]["endCursor"]})).await;
    assert_eq!(page["search"]["nodes"], json!([]));
    metadata_mutations::exercise(
        &router,
        token,
        repository_id.as_str().unwrap(),
        issue_id.as_str().unwrap(),
        pull_id.as_str().unwrap(),
        viewer_id.as_str().unwrap(),
    )
    .await;
    reviews::exercise(&router, token, pull_id.as_str().unwrap()).await;
    for (number, typename) in [(stored.number, "Issue"), (pull.number, "PullRequest")] {
        let data = query(
            &router,
            token,
            include_str!("graphql/gh-issue-view.graphql"),
            json!({"owner":"graphql-alice","repo":"proj","number":number}),
        )
        .await;
        let item = &data["repository"]["issue"];
        assert_eq!(item["__typename"], typename);
        assert_eq!(item["authorAssociation"], "OWNER");
        assert_eq!(item["comments"]["totalCount"], 1);
        assert_eq!(item["comments"]["nodes"][0]["authorAssociation"], "OWNER");
        assert_eq!(item["comments"]["nodes"][0]["viewerDidAuthor"], true);
        assert!(item["comments"]["nodes"][0]["url"]
            .as_str()
            .unwrap()
            .starts_with("http"));
        if typename == "Issue" {
            assert_eq!(item["assignees"]["totalCount"], 1);
            assert_eq!(item["assignees"]["nodes"][0]["login"], "graphql-alice");
            assert_eq!(item["reactionGroups"][0]["content"], "THUMBS_UP");
            assert_eq!(item["reactionGroups"][0]["users"]["totalCount"], 1);
        }
    }
    for (mutation, state) in [("closeIssue", "CLOSED"), ("reopenIssue", "OPEN")] {
        let input = if mutation == "closeIssue" {
            "CloseIssueInput"
        } else {
            "ReopenIssueInput"
        };
        let data = query(&router,token,&format!("mutation($input:{input}!) {{ {mutation}(input:$input) {{ issue {{ id state closedAt }} }} }}"),json!({"input":{"issueId":issue_id}})).await;
        assert_eq!(data[mutation]["issue"]["state"], state);
    }
    let data = query(&router,token,"query($number:Int!) { repository(owner:\"graphql-alice\",name:\"proj\") { issue(number:$number) { id title comments(last:1) { totalCount nodes { body } } } issues(first:1,states:[OPEN]) { totalCount nodes { id } edges { cursor } pageInfo { startCursor endCursor hasNextPage hasPreviousPage } } } }",json!({"number":issue_number})).await;
    assert_eq!(data["repository"]["issue"]["id"], issue_id);
    assert_eq!(data["repository"]["issue"]["comments"]["totalCount"], 1);
    let cursor = data["repository"]["issues"]["edges"][0]["cursor"].clone();
    let data = query(&router,token,"query($after:String) { repository(owner:\"graphql-alice\",name:\"proj\") { issues(first:1,after:$after) { totalCount nodes { id } pageInfo { hasNextPage hasPreviousPage } } } }",json!({"after":cursor})).await;
    assert_eq!(data["repository"]["issues"]["nodes"], json!([]));
    let data = query(&router,token,"query($id:ID!,$ids:[ID!]!) { node(id:$id) { id ... on Repository { name } } nodes(ids:$ids) { id } }",json!({"id":repository_id,"ids":[viewer_id,repository_id,issue_id,pull_id]})).await;
    assert_eq!(data["node"]["name"], "proj");
    assert_eq!(data["nodes"].as_array().unwrap().len(), 4);
    for query_text in ["{ repository(owner:\"graphql-alice\",name:\"proj\") { id issues(first:101) { totalCount } } }","{ repository(owner:\"graphql-alice\",name:\"proj\") { id issues { nodes { id } } } }"] {
        let (_,body) = call(&router,Some(token),query_text,json!({})).await;
        assert!(body["errors"].as_array().is_some_and(|errors| !errors.is_empty()),"{body}");
    }
    let (_,body) = call(&router,Some(token),"{ viewer { login } repository(owner:\"graphql-alice\",name:\"proj\") { name interactionAbility { __typename } } }",json!({})).await;
    assert_eq!(body["data"]["viewer"]["login"], "graphql-alice");
    assert_eq!(body["data"]["repository"]["name"], "proj");
    assert_eq!(
        body["data"]["repository"]["interactionAbility"],
        Value::Null
    );
    assert_eq!(
        body["errors"][0]["message"],
        "Codeyard does not support Repository.interactionAbility yet"
    );
    for query_text in [
        "{ repository(owner:\"graphql-alice\",name:\"proj\") { id } }".to_owned(),
        format!("{{ node(id:{repository_id}) {{ id }} }}"),
    ] {
        let (_, body) = call(&router, Some(&credentials[1]), &query_text, json!({})).await;
        assert_eq!(body["errors"][0]["type"], "NOT_FOUND", "{body}");
    }
    for (mutation, input) in [
        (
            "createIssue",
            json!({"repositoryId":repository_id,"title":"Forbidden"}),
        ),
        (
            "addComment",
            json!({"subjectId":issue_id,"body":"Forbidden"}),
        ),
        ("closeIssue", json!({"issueId":issue_id})),
        ("reopenIssue", json!({"issueId":issue_id})),
    ] {
        let ty = match mutation {
            "createIssue" => "CreateIssueInput",
            "addComment" => "AddCommentInput",
            "closeIssue" => "CloseIssueInput",
            _ => "ReopenIssueInput",
        };
        for credential in [&credentials[1], &credentials[2]] {
            let (_, body) = call(
                &router,
                Some(credential),
                &format!(
                    "mutation($input:{ty}!) {{ {mutation}(input:$input) {{ clientMutationId }} }}"
                ),
                json!({"input":input}),
            )
            .await;
            assert!(
                body["errors"]
                    .as_array()
                    .is_some_and(|errors| !errors.is_empty()),
                "{body}"
            );
        }
    }
    let (_, body) = call(
        &router,
        Some(token),
        "{ viewer { login } repository(owner:\"graphql-alice\",name:\"proj\") { isDisabled } }",
        json!({}),
    )
    .await;
    assert_eq!(body["data"]["viewer"]["login"], "graphql-alice");
    assert_eq!(body["data"]["repository"], Value::Null);
    assert_eq!(
        body["errors"][0]["path"],
        json!(["repository", "isDisabled"])
    );
    let (_, body) = call(
        &router,
        Some(token),
        "query($ids:[ID!]!) { nodes(ids:$ids) { id } }",
        json!({"ids":[viewer_id,"R_AAAAAAAAAAA",issue_id]}),
    )
    .await;
    assert_eq!(
        body["data"]["nodes"],
        json!([{"id":viewer_id},null,{"id":issue_id}])
    );
    assert_eq!(body["errors"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body["errors"][0]["path"], json!(["nodes", 1]));
    let broad = format!(
        "{{ {} }}",
        (0..1001)
            .map(|index| format!("v{index}:viewer {{ login }}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let deep = format!(
        "{{ __type(name:\"Repository\") {{ {} name {} }} }}",
        "ofType {".repeat(25),
        "}".repeat(25)
    );
    for text in [broad, deep] {
        let (_, body) = call(&router, Some(token), &text, json!({})).await;
        assert!(
            body["errors"]
                .as_array()
                .is_some_and(|errors| !errors.is_empty()),
            "{body}"
        );
    }
    for (operation, field, state) in [
        ("PullRequestClose", "closePullRequest", "closed"),
        ("PullRequestReopen", "reopenPullRequest", "open"),
        (
            "PullRequestReadyForReview",
            "markPullRequestReadyForReview",
            "open",
        ),
    ] {
        for credential in [&credentials[1], &credentials[2]] {
            let (_, denied) = request(&router,Some(credential),"POST","/api/graphql",json!({"query":include_str!("graphql/gh-pull-request-state.graphql"),"operationName":operation,"variables":{"input":{"pullRequestId":pull_id}}})).await;
            assert!(denied["errors"].is_array(), "{denied}");
        }
        let (_, result) = request(&router,Some(token),"POST","/api/graphql",json!({"query":include_str!("graphql/gh-pull-request-state.graphql"),"operationName":operation,"variables":{"input":{"pullRequestId":pull_id}}})).await;
        assert!(result.get("errors").is_none(), "{result}");
        assert_eq!(result["data"][field]["pullRequest"]["id"], pull_id);
        assert_eq!(
            db::pull_requests::records::get_pull_request_by_id(f.pool(), pull.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            state
        );
    }
    assert!(
        !db::pull_requests::records::get_pull_request_by_id(f.pool(), pull.id)
            .await
            .unwrap()
            .unwrap()
            .is_draft
    );
    let data = query(&router,token,"mutation($input:UpdateIssueInput!) { updateIssue(input:$input) { issue { title body } actor { login } clientMutationId } }",json!({"input":{"id":issue_id,"title":"Edited issue","clientMutationId":"edit"}})).await;
    assert_eq!(data["updateIssue"]["issue"]["title"], "Edited issue");
    assert_eq!(data["updateIssue"]["actor"]["login"], "graphql-alice");
    assert_eq!(data["updateIssue"]["clientMutationId"], "edit");
    let data = query(&router,token,"mutation($input:UpdatePullRequestInput!) { updatePullRequest(input:$input) { pullRequest { title body } } }",json!({"input":{"pullRequestId":pull_id,"title":"Edited pull request"}})).await;
    assert_eq!(
        data["updatePullRequest"]["pullRequest"]["title"],
        "Edited pull request"
    );
    for (field, ty, expected) in [
        ("addStar", "AddStarInput", true),
        ("removeStar", "RemoveStarInput", false),
    ] {
        let data = query(&router,token,&format!("mutation($input:{ty}!) {{ {field}(input:$input) {{ starrable {{ ... on Repository {{ id viewerHasStarred }} }} }} }}"),json!({"input":{"starrableId":repository_id}})).await;
        assert_eq!(data[field]["starrable"]["viewerHasStarred"], expected);
    }
    for state in ["SUBSCRIBED", "IGNORED", "UNSUBSCRIBED"] {
        let data = query(&router,token,"mutation($input:UpdateSubscriptionInput!) { updateSubscription(input:$input) { subscribable { ... on Repository { viewerSubscription } } } }",json!({"input":{"subscribableId":repository_id,"state":state}})).await;
        assert_eq!(
            data["updateSubscription"]["subscribable"]["viewerSubscription"],
            state
        );
    }
    let bob_node = query(
        &router,
        token,
        "{ user(login:\"graphql-bob\") { id } }",
        json!({}),
    )
    .await["user"]["id"]
        .clone();
    for (field, ty, expected) in [
        ("followUser", "FollowUserInput", true),
        ("unfollowUser", "UnfollowUserInput", false),
    ] {
        let data = query(&router,token,&format!("mutation($input:{ty}!) {{ {field}(input:$input) {{ user {{ login viewerIsFollowing }} }} }}"),json!({"input":{"userId":bob_node}})).await;
        assert_eq!(data[field]["user"]["viewerIsFollowing"], expected);
    }
    for (field, ty, input) in [
        (
            "createPullRequest",
            "CreatePullRequestInput",
            json!({"repositoryId":repository_id,"title":"Denied","baseRefName":"main","headRefName":"feature","maintainerCanModify":false}),
        ),
        (
            "updateIssue",
            "UpdateIssueInput",
            json!({"id":issue_id,"title":"Denied"}),
        ),
        (
            "updatePullRequest",
            "UpdatePullRequestInput",
            json!({"pullRequestId":pull_id,"title":"Denied"}),
        ),
        (
            "mergePullRequest",
            "MergePullRequestInput",
            json!({"pullRequestId":pull_id}),
        ),
        (
            "addStar",
            "AddStarInput",
            json!({"starrableId":repository_id}),
        ),
        (
            "removeStar",
            "RemoveStarInput",
            json!({"starrableId":repository_id}),
        ),
        (
            "updateSubscription",
            "UpdateSubscriptionInput",
            json!({"subscribableId":repository_id,"state":"SUBSCRIBED"}),
        ),
        ("followUser", "FollowUserInput", json!({"userId":bob_node})),
    ] {
        let (_, denied) = call(
            &router,
            Some(&credentials[2]),
            &format!("mutation($input:{ty}!) {{ {field}(input:$input) {{ clientMutationId }} }}"),
            json!({"input":input}),
        )
        .await;
        assert!(denied["errors"].is_array(), "{denied}");
    }
    f.execute(
        "UPDATE pull_requests SET state='merged',merged=TRUE WHERE id=$1",
        &[pull.id],
    )
    .await;
    for (states, total) in [
        (json!(["MERGED"]), 1),
        (json!(["CLOSED"]), 0),
        (json!(["CLOSED", "MERGED"]), 1),
        (json!(["OPEN", "CLOSED", "MERGED"]), 1),
    ] {
        let data = query(&router,token,"query($states:[PullRequestState!]) { repository(owner:\"graphql-alice\",name:\"proj\") { pullRequests(first:10,states:$states) { totalCount nodes { merged state } } } }",json!({"states":states})).await;
        assert_eq!(data["repository"]["pullRequests"]["totalCount"], total);
        assert_eq!(
            data["repository"]["pullRequests"]["nodes"]
                .as_array()
                .unwrap()
                .len(),
            total as usize
        );
    }
    let counts = db::pull_requests::records::count_repo_pull_requests(
        f.pool(),
        repo.id,
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!((counts.open, counts.closed, counts.all), (0, 1, 1));
    db::labels::create_label(f.pool(), alice, repo.id, "enhancement", "00ff00", None)
        .await
        .unwrap();
    let data = query(&router,token,"{ repository(owner:\"graphql-alice\",name:\"proj\") { labels(first:1) { totalCount edges { cursor node { name } } pageInfo { hasNextPage } } } }",json!({})).await;
    assert_eq!(data["repository"]["labels"]["totalCount"], 2);
    assert_eq!(
        data["repository"]["labels"]["pageInfo"]["hasNextPage"],
        true
    );
    let cursor = data["repository"]["labels"]["edges"][0]["cursor"].clone();
    let ordered = query(&router,token,"query($order:LabelOrder) { repository(owner:\"graphql-alice\",name:\"proj\") { labels(first:2,orderBy:$order) { nodes { name } } } }",json!({"order":{"field":"NAME","direction":"DESC"}})).await;
    assert_eq!(
        ordered["repository"]["labels"]["nodes"][0]["name"],
        "enhancement"
    );
    let data = query(&router,token,"query($cursor:String!) { repository(owner:\"graphql-alice\",name:\"proj\") { labels(first:1,after:$cursor) { edges { cursor node { name } } pageInfo { hasPreviousPage hasNextPage } } } }",json!({"cursor":cursor})).await;
    assert_eq!(
        data["repository"]["labels"]["edges"][0]["node"]["name"],
        "enhancement"
    );
    let cursor = data["repository"]["labels"]["edges"][0]["cursor"].clone();
    let data = query(&router,token,"query($cursor:String!) { repository(owner:\"graphql-alice\",name:\"proj\") { labels(last:1,before:$cursor) { nodes { name } } } }",json!({"cursor":cursor})).await;
    assert_eq!(data["repository"]["labels"]["nodes"][0]["name"], "bug");
    let (status, created_ref) = request(
        &router,
        Some(token),
        "POST",
        "/api/v3/repos/graphql-alice/proj/git/refs",
        json!({"ref":"refs/heads/graphql-created","sha":tip}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created_ref}");
    let (status, changed) = request(
        &router,
        Some(token),
        "PUT",
        "/api/v3/repos/graphql-alice/proj/contents/graphql.txt",
        json!({"branch":"graphql-created","message":"GraphQL change","content":"aGVsbG8K"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{changed}");
    let new_pull = query(&router,token,include_str!("graphql/gh-pull-request-create.graphql"),json!({"input":{"repositoryId":repository_id,"title":"Created through GraphQL","headRefName":"graphql-created","baseRefName":"main","maintainerCanModify":true}})).await;
    let created_id = new_pull["createPullRequest"]["pullRequest"]["id"].clone();
    assert!(created_id.is_string(), "{new_pull}");
    let flag = query(
        &router,
        token,
        "query($id:ID!){node(id:$id){...on PullRequest{maintainerCanModify}}}",
        json!({"id":created_id}),
    )
    .await;
    assert_eq!(flag["node"]["maintainerCanModify"], true);
    for enabled in [false, true] {
        let flag=query(&router,token,"mutation($input:UpdatePullRequestInput!){updatePullRequest(input:$input){pullRequest{maintainerCanModify}}}",json!({"input":{"pullRequestId":created_id,"maintainerCanModify":enabled}})).await;
        assert_eq!(
            flag["updatePullRequest"]["pullRequest"]["maintainerCanModify"],
            enabled
        );
    }
    db::repositories::protection::upsert_commit_status(
        f.pool(),
        db::repositories::protection::CommitStatusUpsert {
            repo_id: repo.id,
            sha: changed["commit"]["sha"].as_str().unwrap(),
            context: "graphql-check",
            state: "success",
            description: Some("Passed"),
            target_url: Some("https://checks.example.test/one"),
            creator_id: Some(alice),
        },
    )
    .await
    .unwrap();
    let (status, statuses) = request(
        &router,
        Some(token),
        "GET",
        &format!(
            "/api/v3/repos/graphql-alice/proj/commits/{}/statuses",
            changed["commit"]["sha"].as_str().unwrap()
        ),
        json!(null),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{statuses}");
    let status_node = query(
        &router,
        token,
        "query($id:ID!){node(id:$id){id ...on StatusContext{context state}}}",
        json!({"id":statuses[0]["node_id"]}),
    )
    .await;
    assert_eq!(status_node["node"]["id"], statuses[0]["node_id"]);
    assert_eq!(status_node["node"]["state"], "SUCCESS");
    let reviewed=query(&router,token,"mutation($input:AddPullRequestReviewInput!){addPullRequestReview(input:$input){pullRequestReview{id state body}}}",json!({"input":{"pullRequestId":created_id,"event":"COMMENT","body":"Reviewed through GraphQL"}})).await;
    assert_eq!(
        reviewed["addPullRequestReview"]["pullRequestReview"]["state"],
        "COMMENTED"
    );
    let number = query(
        &router,
        token,
        "query($id:ID!) { node(id:$id) { ... on PullRequest { number } } }",
        json!({"id":created_id}),
    )
    .await["node"]["number"]
        .clone();
    let (status,inline)=request(&router,Some(token),"POST",&format!("/api/v3/repos/graphql-alice/proj/pulls/{number}/comments"),json!({"body":"Inline GraphQL node","path":"graphql.txt","commit_id":changed["commit"]["sha"],"side":"RIGHT","line":1})).await;
    assert_eq!(status, StatusCode::CREATED, "{inline}");
    let inline_node = query(
        &router,
        token,
        "query($id:ID!){node(id:$id){id ...on PullRequestReviewComment{body path line}}}",
        json!({"id":inline["node_id"]}),
    )
    .await;
    assert_eq!(inline_node["node"]["id"], inline["node_id"]);
    assert_eq!(inline_node["node"]["path"], "graphql.txt");
    let details = query(
        &router,
        token,
        include_str!("graphql/gh-pull-request-details.graphql"),
        json!({"owner":"graphql-alice","name":"proj","number":number}),
    )
    .await;
    let details = &details["repository"]["pullRequest"];
    assert_eq!(details["reviews"]["totalCount"], 1);
    assert_eq!(details["latestReviews"]["totalCount"], 1);
    assert_eq!(
        details["latestReviews"]["nodes"][0]["body"],
        "Reviewed through GraphQL"
    );
    assert_eq!(details["reviewRequests"]["totalCount"], 0);
    assert_eq!(details["changedFiles"], 1);
    assert_eq!(details["files"]["nodes"][0]["path"], "graphql.txt");
    assert_eq!(details["files"]["nodes"][0]["changeType"], "ADDED");
    assert_eq!(details["commits"]["totalCount"], 1);
    assert_eq!(
        details["commits"]["nodes"][0]["commit"]["oid"],
        changed["commit"]["sha"]
    );
    assert_eq!(
        details["statusCheckRollup"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]
            ["nodes"][0]["state"],
        "SUCCESS"
    );
    for (id, typename) in [
        (
            changed["commit"]["node_id"].as_str().unwrap().to_owned(),
            "Commit",
        ),
        (
            node_id::git("B", &format!("{}/{}", repo.id, changed["content"]["sha"].as_str().unwrap())),
            "Blob",
        ),
        (
            node_id::reference(repo.id, "refs/heads/graphql-created"),
            "Ref",
        ),
    ] {
        let node = query(
            &router,
            token,
            "query($id:ID!){node(id:$id){id __typename ...on Blob{text}}}",
            json!({"id":id}),
        )
        .await;
        assert_eq!(node["node"]["__typename"], typename);
        assert_eq!(node["node"]["id"], id);
        if typename == "Blob" {
            assert_eq!(node["node"]["text"], "hello\n");
        }
    }
    let (status, tree) = request(
        &router,
        Some(token),
        "GET",
        &format!(
            "/api/v3/repos/graphql-alice/proj/git/trees/{}",
            changed["commit"]["sha"].as_str().unwrap()
        ),
        json!(null),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tree}");
    let tree_id = tree["node_id"].as_str().unwrap().to_owned();
    let object = query(
        &router,
        token,
        "query($id:ID!){node(id:$id){id ...on Tree{oid entries{name nameRaw path pathRaw type submodule{gitUrl} object{oid}}}}}",
        json!({"id":tree_id}),
    )
    .await;
    assert_eq!(object["node"]["id"], tree_id);
    let entries = object["node"]["entries"].as_array().unwrap();
    assert!(!entries.is_empty());
    let first = &entries[0];
    use base64::Engine as _;
    let raw = |value: &Value| {
        String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(value.as_str().unwrap())
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(raw(&first["nameRaw"]), first["name"].as_str().unwrap());
    assert_eq!(raw(&first["pathRaw"]), first["path"].as_str().unwrap());
    assert!(first["submodule"].is_null());
    let(status,tag)=request(&router,Some(token),"POST","/api/v3/repos/graphql-alice/proj/git/tags",json!({"tag":"graphql-node-tag","message":"GraphQL tag","object":changed["commit"]["sha"],"type":"commit"})).await;
    assert_eq!(status, StatusCode::CREATED, "{tag}");
    let object = query(
        &router,
        token,
        "query($id:ID!){node(id:$id){id ...on Tag{name target{oid}}}}",
        json!({"id":tag["node_id"]}),
    )
    .await;
    assert_eq!(object["node"]["id"], tag["node_id"]);
    assert_eq!(object["node"]["target"]["oid"], changed["commit"]["sha"]);
    let updated_branch=query(&router,token,"mutation($input:UpdatePullRequestBranchInput!){updatePullRequestBranch(input:$input){pullRequest{headRefOid}}}",json!({"input":{"pullRequestId":created_id,"expectedHeadOid":changed["commit"]["sha"]}})).await;
    let merge_head = updated_branch["updatePullRequestBranch"]["pullRequest"]["headRefOid"].clone();
    for expected in [
        json!("0000000000000000000000000000000000000000"),
        json!(123),
    ] {
        let (_, mismatch) = call(
            &router,
            Some(token),
            include_str!("graphql/gh-pull-request-merge.graphql"),
            json!({"input":{"pullRequestId":created_id,"expectedHeadOid":expected}}),
        )
        .await;
        assert!(mismatch["errors"].is_array(), "{mismatch}");
    }
    let merged = query(&router,token,include_str!("graphql/gh-pull-request-merge.graphql"),json!({"input":{"pullRequestId":created_id,"expectedHeadOid":merge_head,"clientMutationId":"merged"}})).await;
    assert_eq!(merged["mergePullRequest"]["clientMutationId"], "merged");
    let merged = query(
        &router,
        token,
        "query($id:ID!) { node(id:$id) { ... on PullRequest { merged state } } }",
        json!({"id":created_id}),
    )
    .await;
    assert_eq!(merged["node"]["merged"], true);
    assert_eq!(merged["node"]["state"], "MERGED");
    objects::exercise(f.pool(), &router, token, &credentials[1], repo.id, alice).await;
    git_lane_nodes::exercise(
        &state,
        &router,
        (token, &credentials[1]),
        (alice, org.id),
        changed["commit"]["sha"].as_str().unwrap(),
    )
    .await;
    maintainers::exercise(
        &state,
        &router,
        (token, &credentials[1]),
        repo.id,
        (alice, bob),
    )
    .await;
    repository_mutations::exercise(
        &router,
        (token, &credentials[1]),
        node_id::encode("R", repo.id).into(),
    )
    .await;
    f.execute(
        "UPDATE graphql_rate_limits SET used=5000 WHERE token_id=$1",
        &[token_ids[0]],
    )
    .await;
    let (_, body) = call(&router, Some(token), "{ rateLimit { used } }", json!({})).await;
    assert_eq!(body["errors"][0]["message"], "API rate limit exceeded");
    f.execute(
        "UPDATE graphql_rate_limits SET reset_at='2000-01-01T00:00:00Z' WHERE token_id=$1",
        &[token_ids[0]],
    )
    .await;
    let data = query(
        &router,
        token,
        "{ rateLimit { used remaining } }",
        json!({}),
    )
    .await;
    assert_eq!(data["rateLimit"]["used"], 1);
    assert_eq!(data["rateLimit"]["remaining"], 4999);
}

#[tokio::test]
async fn github_graphql_interface_outputs_keep_declared_nullability() {
    use async_graphql::{Context, EmptyMutation, EmptySubscription, Interface, Object, Schema};
    struct StrongUser;
    #[Object]
    impl StrongUser {
        async fn email(&self, _ctx: &Context<'_>) -> String {
            "mail@example.test".into()
        }
    }
    #[derive(Interface)]
    #[graphql(field(name = "email", ty = "Option<String>"))]
    enum Profile {
        User(StrongUser),
    }
    struct Query;
    #[Object]
    impl Query {
        async fn profile(&self) -> Profile {
            StrongUser.into()
        }
    }
    // A weaker object return type cannot implement the interface's Rust method.
    let schema = Schema::build(Query, EmptyMutation, EmptySubscription).finish();
    let result = schema.execute("{ profile { email } object:__type(name:\"StrongUser\") { fields { type { kind } } } interface:__type(name:\"Profile\") { fields { type { kind } } } }").await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let data = result.data.into_json().unwrap();
    assert_eq!(data["profile"]["email"], "mail@example.test");
    assert_eq!(data["object"]["fields"][0]["type"]["kind"], "NON_NULL");
    assert_eq!(data["interface"]["fields"][0]["type"]["kind"], "SCALAR");
}

#[tokio::test]
async fn sqlite_github_graphql_contract() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_graphql_contract() {
    contract(true).await;
}

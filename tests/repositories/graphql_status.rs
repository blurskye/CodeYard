use super::{call, query};
use crate::support::database::Database;
use crate::support::http_client::Client;
use crate::support::organizations::applied;
use axum::http::StatusCode;

use codeyard_repos_core::repositories::lifecycle::provision;
use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
use codeyard_db_repos::repositories::creation::RepositoryTarget;
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};

// status/status.go's searchQuery, including both fragments and its leading newline.
const ASSIGNED_SEARCH: &str = include_str!("graphql_queries/gh_status.graphql");
// Generated from queries_user.go's CurrentLoginName and feature_detection.go's SearchFeatures.
const USER_CURRENT: &str = "query UserCurrent{viewer{login}}";
const SEARCH_TYPES: &str = "query SearchType_enumValues{SearchType: __type(name: \"SearchType\"){enumValues(includeDeprecated: true){name}}}";

fn variables(suffix: &str) -> Value {
    json!({
        "searchAssigns":format!("assignee:@me state:open archived:false{suffix}"),
        "searchReviews":format!("review-requested:@me state:open archived:false{suffix}")
    })
}

async fn discussions(
    f: &Database,
    repo: i64,
    author: i64,
    viewer: i64,
    name: &str,
) -> (Value, Value, i64, i64) {
    let issue = codeyard_db_repos::issues::create_issue(
        f.pool(),
        repo,
        author,
        "Assigned issue",
        "Issue body",
    )
    .await
    .unwrap();
    let head_branch = format!("feature-{}", issue.number);
    let pull = codeyard_db_repos::pull_requests::records::create_pull_request(
        f.pool(),
        codeyard_db_repos::pull_requests::records::NewPullRequest {
            issue: None,
            maintainer_can_modify: false,
            repo_id: repo,
            head_repo_id: Some(repo),
            author_id: author,
            title: "Assigned pull",
            body: "Pull body",
            head_branch: &head_branch,
            head_sha: "1111111111111111111111111111111111111111",
            base_branch: "main",
            base_sha: "2222222222222222222222222222222222222222",
            is_draft: false,
        },
    )
    .await
    .unwrap();
    f.execute(
        "INSERT INTO issue_assignees (issue_id,user_id) VALUES ($1,$2)",
        &[issue.id, viewer],
    )
    .await;
    f.execute(
        "INSERT INTO pr_assignees (pull_request_id,user_id) VALUES ($1,$2)",
        &[pull.id, viewer],
    )
    .await;
    f.execute(
        "INSERT INTO pr_reviewers (pull_request_id,user_id) VALUES ($1,$2)",
        &[pull.id, viewer],
    )
    .await;
    (
        json!({"__typename":"Issue","updatedAt":issue.updated_at,"title":issue.title,
            "number":issue.number,"repository":{"nameWithOwner":name}}),
        json!({"__typename":"PullRequest","updatedAt":pull.updated_at,"title":pull.title,
            "number":pull.number,"repository":{"nameWithOwner":name}}),
        issue.id,
        pull.id,
    )
}

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("status-alice").await;
    let bob = f.actor("status-bob").await;
    let root = tempfile::tempdir().unwrap();
    let org = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            f.pool(),
            alice,
            &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
                "status-org",
                codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let mut assignments = Vec::new();
    let mut reviews = Vec::new();
    let mut work_pull = 0;
    for (name, owner, owner_name) in [
        (
            "work",
            RepositoryCreationOwner::personal(alice),
            "status-alice",
        ),
        (
            "excluded",
            RepositoryCreationOwner::personal(alice),
            "status-alice",
        ),
        (
            "archived",
            RepositoryCreationOwner::personal(alice),
            "status-alice",
        ),
        (
            "secret",
            RepositoryCreationOwner::personal(bob),
            "status-bob",
        ),
        (
            "project",
            RepositoryCreationOwner {
                actor_id: alice,
                target: RepositoryTarget::Organization(org.id),
            },
            "status-org",
        ),
    ] {
        let repo = provision((f.pool(), None), root.path(), owner, name, None, true, None)
            .await
            .unwrap();
        if owner_name != "status-bob" {
            f.execute(
                "INSERT INTO repo_access (repo_id,user_id,access_level) VALUES ($1,$2,3)",
                &[repo.id, bob],
            )
            .await;
        }
        let full_name = format!("{owner_name}/{name}");
        let (issue, pull, _, pull_id) = discussions(&f, repo.id, bob, alice, &full_name).await;
        match name {
            "archived" => {
                f.execute(
                    "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
                    &[repo.id],
                )
                .await;
            }
            "secret" => {}
            _ => {
                assignments.extend([issue, pull.clone()]);
                reviews.push(pull);
            }
        }
        if name == "work" {
            work_pull = pull_id;
            let (_, _, closed_issue, closed_pull) =
                discussions(&f, repo.id, bob, alice, &full_name).await;
            f.execute(
                "UPDATE issues SET state='closed' WHERE id=$1",
                &[closed_issue],
            )
            .await;
            f.execute(
                "UPDATE pull_requests SET state='closed' WHERE id=$1",
                &[closed_pull],
            )
            .await;
            codeyard_db_repos::issues::create_issue(f.pool(), repo.id, bob, "Unassigned issue", "")
                .await
                .unwrap();
        }
    }
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), alice).await;
    let token = tokens::create(
        f.pool(),
        alice,
        Some(&session.session),
        TokenInput {
            name: "status",
            scope: "read",
            expires_at: None,
        },
    )
    .await
    .unwrap()
    .value;
    let data = query(&router, &token, USER_CURRENT, json!({})).await;
    assert_eq!(data, json!({"viewer":{"login":"status-alice"}}));
    let data = query(&router, &token, SEARCH_TYPES, json!({})).await;
    let actual = data["SearchType"]["enumValues"].as_array().unwrap();
    let expected: Vec<_> = [
        "DISCUSSION",
        "ISSUE",
        "ISSUE_ADVANCED",
        "ISSUE_HYBRID",
        "ISSUE_SEMANTIC",
        "REPOSITORY",
        "USER",
    ]
    .into_iter()
    .map(|name| json!({"name":name}))
    .collect();
    assert_eq!(actual, &expected);
    let data = query(&router, &token, ASSIGNED_SEARCH, variables("")).await;
    assert_eq!(
        data,
        json!({"assignments":{"nodes":assignments},"reviewRequested":{"nodes":reviews}})
    );
    let data = query(
        &router,
        &token,
        ASSIGNED_SEARCH,
        variables(" org:status-org"),
    )
    .await;
    assert_eq!(
        data,
        json!({"assignments":{"nodes":&assignments[4..]},"reviewRequested":{"nodes":&reviews[2..]}})
    );
    let data = query(
        &router,
        &token,
        ASSIGNED_SEARCH,
        variables(" -repo:status-alice/work -repo:status-alice/excluded"),
    )
    .await;
    assert_eq!(
        data,
        json!({"assignments":{"nodes":&assignments[4..]},"reviewRequested":{"nodes":&reviews[2..]}})
    );
    let data = query(
        &router,
        &token,
        ASSIGNED_SEARCH,
        variables(" org:status-org -repo:status-org/project"),
    )
    .await;
    assert_eq!(
        data,
        json!({"assignments":{"nodes":[]},"reviewRequested":{"nodes":[]}})
    );
    let mut vars = variables("");
    vars["limit"] = json!(1);
    let data = query(&router, &token, ASSIGNED_SEARCH, vars).await;
    assert_eq!(
        data,
        json!({"assignments":{"nodes":&assignments[..1]},"reviewRequested":{"nodes":&reviews[..1]}})
    );
    codeyard_db_repos::pull_requests::reviews::remove_pull_request_reviewer(
        f.pool(),
        work_pull,
        alice,
        alice,
    )
    .await
    .unwrap();
    let data = query(&router, &token, ASSIGNED_SEARCH, variables("")).await;
    assert_eq!(data["reviewRequested"]["nodes"], json!(&reviews[1..]));
    let mut vars = variables("");
    vars["limit"] = json!(101);
    let (status, body) = call(&router, Some(&token), ASSIGNED_SEARCH, vars).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["errors"][0]["message"],
        "first and last must be between 0 and 100"
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_gh_status_and_search_queries() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_gh_status_and_search_queries() {
    contract(true).await;
}

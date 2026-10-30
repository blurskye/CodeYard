//! Requests and response fields used by pkg/cmd/status and pkg/search in gh.
use super::github::{document, request};
use crate::support::{database::Database, http_client::Client};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::Response,
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use codeyard::backend::{
    db,
    identity::tokens::{self, TokenInput},
    repositories::{lifecycle, store::repo_store, templates},
};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn token(f: &Database, router: &Router, actor: i64) -> String {
    let browser = Client::login(router, f.pool(), actor).await;
    let token = tokens::create(
        f.pool(),
        actor,
        Some(&browser.session),
        TokenInput {
            name: "gh",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    format!("Bearer {}", token.value)
}
async fn get(router: &Router, path: &str, token: &str, code: bool) -> Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("authorization", token)
                .header(
                    "accept",
                    if code {
                        "application/vnd.github.text-match+json"
                    } else {
                        "application/vnd.github.v3+json"
                    },
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}
fn search_path(kind: &str, query: &str, extra: &[(&str, &str)]) -> String {
    let mut params = url::form_urlencoded::Serializer::new(String::new());
    params
        .append_pair("page", "1")
        .append_pair("per_page", "30")
        .append_pair("q", query)
        .append_pair("order", "desc");
    for (k, v) in extra {
        params.append_pair(k, v);
    }
    format!("/api/v3/search/{kind}?{}", params.finish())
}
async fn search(
    f: &Database,
    router: &Router,
    token: &str,
    kind: &str,
    query: &str,
    extra: &[(&str, &str)],
) -> Value {
    // Each row is an independent CLI invocation in a fresh rate-limit window.
    f.execute(
        "UPDATE github_api_limits SET reset_at=0 WHERE resource='search'",
        &[],
    )
    .await;
    let op = match kind {
        "repositories" => "search/repos",
        "issues" => "search/issues-and-pull-requests",
        "commits" => "search/commits",
        "code" => "search/code",
        _ => unreachable!(),
    };
    document(
        get(
            router,
            &search_path(kind, query, extra),
            token,
            kind == "code",
        )
        .await,
        op,
        200,
    )
    .await
}
fn timestamp(value: &Value) {
    chrono::DateTime::parse_from_rfc3339(value.as_str().unwrap()).unwrap();
}
fn person(value: &Value, login: &str) {
    assert_eq!(value["login"], login);
    assert!(!value["node_id"].as_str().unwrap().is_empty());
    assert_eq!(value["type"], "User");
    assert_eq!(value["site_admin"], false);
    assert!(value["html_url"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/{login}")));
    assert!(value["gravatar_id"].is_string());
}
fn repository(value: &Value) {
    assert_eq!(value["name"], "proj");
    assert_eq!(value["full_name"], "alice/proj");
    assert_eq!(value["description"], "Needle repository");
    assert_eq!(value["default_branch"], "main");
    assert!(!value["node_id"].as_str().unwrap().is_empty());
    assert!(value["html_url"].as_str().unwrap().ends_with("/alice/proj"));
    person(&value["owner"], "alice");
    for key in ["created_at", "updated_at", "pushed_at"] {
        timestamp(&value[key]);
    }
    for key in ["has_downloads", "has_issues"] {
        assert_eq!(value[key], true, "{key}");
    }
    for key in [
        "has_pages",
        "has_projects",
        "has_wiki",
        "archived",
        "disabled",
        "fork",
        "private",
    ] {
        assert_eq!(value[key], false, "{key}");
    }
    assert_eq!(value["homepage"], Value::Null);
    assert_eq!(value["visibility"], "public");
    assert_eq!(value["language"], "Rust");
    assert_eq!(value["license"]["key"], "mit");
    assert_eq!(value["license"]["name"], "MIT");
    assert!(value["license"]["url"]
        .as_str()
        .unwrap()
        .ends_with("/licenses/mit"));
    for key in [
        "forks_count",
        "size",
        "stargazers_count",
        "open_issues_count",
        "watchers_count",
    ] {
        assert!(value[key].is_number(), "{key}");
    }
}
async fn save(browser: &Client, path: &str, text: &str, tip: &str, message: &str) -> String {
    browser.expect("POST",&format!("/api/repos/alice/proj/contents/{path}"),json!({"content":STANDARD.encode(text),"message":message,"branch":"main","expected_revision":tip}),StatusCode::CREATED).await["sha"].as_str().unwrap().to_owned()
}
async fn repositories_and_content(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision_with_initial_files(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        Some("Needle repository"),
        false,
        lifecycle::InitialFiles {
            readme: true,
            gitignore: None,
            license: templates::license("MIT"),
        },
    )
    .await
    .unwrap();
    let hidden = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "hidden",
        Some("Needle repository"),
        true,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), alice).await;
    let auth = token(&f, &router, alice).await;
    let outsider = token(&f, &router, bob).await;
    let initial = repo_store(&repo, None).commit("main").await.unwrap();
    let parent = save(
        &browser,
        "src/main.rs",
        "// Needle 道路\nfn main() {}\n",
        &initial.sha,
        "Needle first",
    )
    .await;
    let tip = save(
        &browser,
        "src/lib.rs",
        "// Needle library\n",
        &parent,
        "Needle second",
    )
    .await;
    let commit = repo_store(&repo, None).commit(&tip).await.unwrap();
    f.execute(
        "UPDATE repositories SET size_bytes=4096 WHERE id=$1",
        &[repo.id],
    )
    .await;
    f.execute("INSERT INTO repo_languages(repo_id,language,bytes,percentage,color) VALUES($1,'Rust',100,100,'#dea584') ON CONFLICT(repo_id,language) DO UPDATE SET bytes=100",&[repo.id]).await;
    document(
        request(
            &router,
            "PUT",
            "/api/v3/repos/alice/proj/topics",
            Some(&auth),
            json!({"names":["rust","forge"]}),
        )
        .await,
        "repos/replace-all-topics",
        200,
    )
    .await;
    let issue = db::issues::create_issue(f.pool(), repo.id, alice, "Starter task", "")
        .await
        .unwrap();
    for name in ["good first issue", "help wanted"] {
        let label = db::labels::create_label(f.pool(), alice, repo.id, name, "123456", None)
            .await
            .unwrap();
        db::labels::add_issue_label(f.pool(), alice, repo.id, issue.id, label.id)
            .await
            .unwrap();
    }
    let value = search(&f, &router, &auth, "repositories", "Needle", &[]).await;
    assert_eq!(value["total_count"], 2);
    assert_eq!(value["incomplete_results"], false);
    repository(
        value["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "proj")
            .unwrap(),
    );
    for (query, total) in [
        ("archived:false", 1),
        ("archived:true", 0),
        ("created:>=2000-01-01", 1),
        ("followers:0", 1),
        ("followers:>0", 0),
        ("fork:false", 1),
        ("fork:true", 1),
        ("fork:only", 0),
        ("forks:0", 1),
        ("forks:>0", 0),
        ("good-first-issues:1", 1),
        ("good-first-issues:>1", 0),
        ("help-wanted-issues:1", 1),
        ("help-wanted-issues:0", 0),
        ("Needle in:name", 0),
        ("Needle in:description", 1),
        ("Needle in:readme", 1),
        ("missing in:readme", 0),
        ("is:public", 1),
        ("is:private", 0),
        ("is:internal", 0),
        ("is:private is:public", 1),
        ("language:Rust", 1),
        ("language:Python", 0),
        ("license:mit", 1),
        ("license:apache-2.0", 0),
        ("pushed:>=2000-01-01", 1),
        ("pushed:<2000-01-01", 0),
        ("size:4", 1),
        ("size:>4", 0),
        ("stars:0", 1),
        ("stars:>0", 0),
        ("topic:rust topic:forge", 1),
        ("topic:missing", 0),
        ("topics:2", 1),
        ("topics:>2", 0),
        ("user:alice", 1),
        ("user:bob", 0),
    ] {
        let value = search(
            &f,
            &router,
            &outsider,
            "repositories",
            &format!("repo:alice/proj {query}"),
            &[],
        )
        .await;
        assert_eq!(value["total_count"], total, "{query}: {value}");
        assert_eq!(value["incomplete_results"], false, "{query}: {value}");
    }
    for sort in ["stars", "forks", "updated", "help-wanted-issues"] {
        let value = search(
            &f,
            &router,
            &auth,
            "repositories",
            "repo:alice/proj",
            &[("sort", sort)],
        )
        .await;
        assert_eq!(value["total_count"], 1);
    }
    let value = search(
        &f,
        &router,
        &outsider,
        "repositories",
        "license:mit -repo:alice/proj",
        &[],
    )
    .await;
    assert_eq!(value["total_count"], 0);
    let code=search(&f,&router,&auth,"code","Needle extension:rs filename:main.rs in:file language:Rust repo:alice/proj size:<100 user:alice",&[]).await;
    assert_eq!(code["total_count"], 1);
    assert_eq!(code["incomplete_results"], false);
    let file = &code["items"][0];
    assert_eq!(file["name"], "main.rs");
    assert_eq!(file["path"], "src/main.rs");
    assert_eq!(file["sha"].as_str().unwrap().len(), 40);
    assert!(file["html_url"].as_str().unwrap().ends_with("/src/main.rs"));
    repository(&file["repository"]);
    let fragment = &file["text_matches"][0];
    assert_eq!(fragment["fragment"], "// Needle 道路");
    assert_eq!(fragment["object_type"], "FileContent");
    assert_eq!(fragment["property"], "content");
    assert_eq!(
        fragment["matches"],
        json!([{"text":"Needle","indices":[3,9]}])
    );
    for (query, total) in [
        ("extension:txt", 0),
        ("filename:missing.rs", 0),
        ("language:Python", 0),
        ("size:>100 extension:rs", 0),
        ("size:>100 filename:LICENSE", 1),
        ("user:bob", 0),
        ("src in:path", 2),
        ("Needle in:file in:path", 3),
    ] {
        let code = search(
            &f,
            &router,
            &auth,
            "code",
            &format!("repo:alice/proj {query}"),
            &[("sort", "indexed")],
        )
        .await;
        assert_eq!(code["total_count"], total, "{query}: {code}");
    }
    let query=format!("Needle author:alice author-date:>=2000-01-01 author-email:{} author-name:alice committer:alice committer-date:>=2000-01-01 committer-email:{} committer-name:alice hash:{tip} is:public merge:false parent:{parent} repo:alice/proj tree:{} user:alice",commit.author_email,commit.committer_email,commit.tree_sha);
    let found = search(
        &f,
        &router,
        &auth,
        "commits",
        &query,
        &[("sort", "author-date")],
    )
    .await;
    assert_eq!(found["total_count"], 1, "{found}");
    assert_eq!(found["incomplete_results"], false);
    let hit = &found["items"][0];
    person(&hit["author"], "alice");
    person(&hit["committer"], "alice");
    assert_eq!(hit["sha"], tip);
    assert!(!hit["node_id"].as_str().unwrap().is_empty());
    assert!(hit["html_url"].as_str().unwrap().ends_with(&tip));
    assert_eq!(hit["parents"][0]["sha"], parent);
    assert!(hit["parents"][0]["html_url"]
        .as_str()
        .unwrap()
        .ends_with(&parent));
    assert_eq!(
        hit["commit"]["message"].as_str().unwrap().trim(),
        "Needle second"
    );
    assert_eq!(hit["commit"]["comment_count"], 0);
    assert_eq!(hit["commit"]["tree"]["sha"], commit.tree_sha);
    for who in ["author", "committer"] {
        assert_eq!(hit["commit"][who]["name"], "alice");
        assert_eq!(hit["commit"][who]["email"], commit.author_email);
        timestamp(&hit["commit"][who]["date"]);
    }
    repository(&hit["repository"]);
    for query in [
        "author:bob",
        "author-date:<2000-01-01",
        "author-email:missing",
        "author-name:missing",
        "committer:bob",
        "committer-date:<2000-01-01",
        "committer-email:missing",
        "committer-name:missing",
        "hash:0000000",
        "is:private",
        "is:internal",
        "merge:true",
        "parent:0000000",
        "tree:0000000",
        "user:bob",
    ] {
        let value = search(
            &f,
            &router,
            &auth,
            "commits",
            &format!("repo:alice/proj {query}"),
            &[("sort", "committer-date")],
        )
        .await;
        assert_eq!(value["total_count"], 0, "{query}: {value}");
    }
    let hidden_issue = db::issues::create_issue(f.pool(), hidden.id, alice, "Secret Needle", "")
        .await
        .unwrap();
    assert!(hidden_issue.id > 0);
    let value = search(&f, &router, &outsider, "commits", "repo:alice/hidden", &[]).await;
    assert_eq!(value["total_count"], 0);
    f.finish().await;
}

async fn discussions(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let issue = db::issues::create_issue(
        f.pool(),
        repo.id,
        alice,
        "Needle issue",
        "Needle @bob @yard/reviewers",
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
            title: "Needle pull",
            body: "Needle @bob @yard/reviewers",
            head_branch: "topic",
            head_sha: &"a".repeat(40),
            base_branch: "main",
            base_sha: &"b".repeat(40),
            is_draft: true,
        },
    )
    .await
    .unwrap();
    db::issues::add_issue_comment(f.pool(), issue.repo_id, issue.id, bob, "Needle comment", &codeyard::backend::db::users::accounts::AccountCredential::Trusted)
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    db::pull_requests::reviews::add_pull_request_review(
        f.pool(),
        pull.id,
        bob,
        "approved",
        Some("Needle review"),
        Some(&pull.head_sha),
    )
    .await
    .unwrap();
    f.execute(
        "INSERT INTO pr_reviewers(pull_request_id,user_id) VALUES($1,$2)",
        &[pull.id, bob],
    )
    .await;
    for item in [
        db::assignees::AssignedItem::Issue(issue.id),
        db::assignees::AssignedItem::PullRequest(pull.id),
    ] {
        db::assignees::assign_user(f.pool(), alice, repo.id, item, bob)
            .await
            .unwrap();
    }
    let router = f.router(root.path()).await;
    let auth = token(&f, &router, alice).await;
    document(
        request(
            &router,
            "POST",
            "/api/v3/repos/alice/proj/labels",
            Some(&auth),
            json!({"name":"bug","color":"123456","description":"A bug"}),
        )
        .await,
        "issues/create-label",
        201,
    )
    .await;
    let milestone = document(
        request(
            &router,
            "POST",
            "/api/v3/repos/alice/proj/milestones",
            Some(&auth),
            json!({"title":"Soon"}),
        )
        .await,
        "issues/create-milestone",
        201,
    )
    .await;
    for number in [issue.number, pull.number] {
        document(
            request(
                &router,
                "PATCH",
                &format!("/api/v3/repos/alice/proj/issues/{number}"),
                Some(&auth),
                json!({"labels":["bug"],"milestone":milestone["number"]}),
            )
            .await,
            "issues/update",
            200,
        )
        .await;
    }
    let project = db::projects::create(
        f.pool(),
        alice,
        db::projects::Owner::User(alice),
        "Search board",
        None,
        false,
    )
    .await
    .unwrap();
    db::projects::add_item(
        f.pool(),
        alice,
        project.id,
        &db::projects::Content::Issue(issue.id),
    )
    .await
    .unwrap();
    db::projects::add_item(
        f.pool(),
        alice,
        project.id,
        &db::projects::Content::PullRequest(pull.id),
    )
    .await
    .unwrap();
    f.execute("INSERT INTO commit_statuses(repo_id,sha,context,state) VALUES($1,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','ci','success')",&[repo.id]).await;
    f.execute("INSERT INTO repo_languages(repo_id,language,bytes,percentage,color) VALUES($1,'Rust',100,100,'#dea584')",&[repo.id]).await;
    for (kind, number) in [("issue", issue.number), ("pr", pull.number)] {
        let query=format!("( Needle ) archived:false assignee:bob author:alice commenter:bob comments:>=0 created:>=2000-01-01 (in:title OR in:body) interactions:>=0 involves:bob is:public label:bug language:Rust mentions:bob milestone:Soon project:alice/{} reactions:0 repo:alice/proj state:open type:{kind} updated:>=2000-01-01 user:alice",project.number);
        let value = search(
            &f,
            &router,
            &auth,
            "issues",
            &query,
            &[("advanced_search", "true"), ("sort", "updated")],
        )
        .await;
        assert_eq!(value["total_count"], 1, "{query}: {value}");
        assert_eq!(value["incomplete_results"], false);
        let item = &value["items"][0];
        assert_eq!(item["number"], number);
        assert_eq!(
            item["title"],
            if kind == "pr" {
                "Needle pull"
            } else {
                "Needle issue"
            }
        );
        assert!(item["body"].as_str().unwrap().starts_with("Needle"));
        assert!(!item["node_id"].as_str().unwrap().is_empty());
        assert_eq!(item["state"], "open");
        assert_eq!(item["locked"], false);
        assert_eq!(item["author_association"], "OWNER");
        assert_eq!(item["closed_at"], Value::Null);
        assert!(item["comments"].is_number());
        for key in ["created_at", "updated_at"] {
            timestamp(&item[key]);
        }
        person(&item["user"], "alice");
        person(&item["assignees"][0], "bob");
        let label = &item["labels"][0];
        assert_eq!(label["name"], "bug");
        assert_eq!(label["color"], "123456");
        assert_eq!(label["description"], "A bug");
        assert!(label["node_id"].is_string());
        assert!(item["repository_url"]
            .as_str()
            .unwrap()
            .ends_with("/repos/alice/proj"));
        assert!(item["html_url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/{number}")));
        if kind == "pr" {
            assert_eq!(item["draft"], true);
            assert!(item["pull_request"]["html_url"]
                .as_str()
                .unwrap()
                .ends_with(&format!("/pull/{number}")));
            assert!(item["pull_request"]["merged_at"].is_null());
        } else {
            assert!(item["pull_request"].is_null());
        }
    }
    let value = search(&f, &router, &auth, "issues", "author:app/unknown", &[]).await;
    assert_eq!(value["total_count"], 0);
    for (query, total) in [
        ("base:main", 1),
        ("base:missing", 0),
        ("head:topic", 1),
        ("head:top", 1),
        ("head:missing", 0),
        ("draft:true", 1),
        ("draft:false", 0),
        ("is:unmerged", 1),
        ("is:merged", 0),
        ("review:approved", 1),
        ("review:changes_requested", 0),
        ("review:required", 1),
        ("review:none", 0),
        ("review-requested:bob", 1),
        ("review-requested:alice", 0),
        ("reviewed-by:bob", 1),
        ("reviewed-by:alice", 0),
        ("status:success", 1),
        ("status:failure", 0),
        ("status:pending", 0),
        ("merged:>=2000-01-01", 0),
        ("closed:>=2000-01-01", 0),
    ] {
        let value = search(
            &f,
            &router,
            &auth,
            "issues",
            &format!("repo:alice/proj type:pr {query}"),
            &[],
        )
        .await;
        assert_eq!(value["total_count"], total, "{query}: {value}");
    }
    for query in [
        "no:assignee",
        "no:label",
        "no:milestone",
        "no:project",
        "is:locked",
        "is:internal",
        "archived:true",
        "assignee:alice",
        "author:bob",
        "commenter:alice",
        "comments:>10",
        "created:<2000-01-01",
        "interactions:>10",
        "involves:missing",
        "label:missing",
        "language:Python",
        "mentions:alice",
        "milestone:missing",
        "project:alice/999",
        "reactions:>0",
        "state:closed",
        "updated:<2000-01-01",
        "user:bob",
    ] {
        let value = search(
            &f,
            &router,
            &auth,
            "issues",
            &format!("repo:alice/proj {query}"),
            &[],
        )
        .await;
        assert_eq!(value["total_count"], 0, "{query}: {value}");
    }
    let org = match db::organizations::create_organization(
        f.pool(),
        alice,
        &db::organizations::NewOrganization::new(
            "yard",
            db::organizations::OrganizationProfile::new(None, None).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    {
        db::organizations::OrganizationOutcome::Applied(org) => org,
        _ => panic!("organization"),
    };
    let org_repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner {
            actor_id: alice,
            target: db::repositories::creation::RepositoryTarget::Organization(org.id),
        },
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let org_pull = db::pull_requests::records::create_pull_request(
        f.pool(),
        db::pull_requests::records::NewPullRequest {
            issue: None,
            maintainer_can_modify: false,
            repo_id: org_repo.id,
            author_id: alice,
            head_repo_id: None,
            title: "Team pull",
            body: "Hello @yard/reviewers @yard/hidden",
            head_branch: "topic",
            head_sha: &"a".repeat(40),
            base_branch: "main",
            base_sha: &"b".repeat(40),
            is_draft: false,
        },
    )
    .await
    .unwrap();
    for (slug, privacy) in [("reviewers", "closed"), ("hidden", "secret")] {
        let team = document(
            request(
                &router,
                "POST",
                "/api/v3/orgs/yard/teams",
                Some(&auth),
                json!({"name":slug,"privacy":privacy,"repo_names":["yard/proj"]}),
            )
            .await,
            "teams/create",
            201,
        )
        .await;
        f.execute(
            "INSERT INTO pr_team_reviewers(pull_request_id,team_id) VALUES($1,$2)",
            &[org_pull.id, team["id"].as_i64().unwrap()],
        )
        .await;
    }
    for (query, total) in [
        ("team:yard/reviewers", 1),
        ("team:yard/missing", 0),
        ("team-review-requested:yard/reviewers", 1),
        ("team-review-requested:yard/missing", 0),
        ("team-review-requested:yard/hidden", 1),
        ("team:yard/hidden", 1),
        ("user:yard", 1),
    ] {
        let value = search(
            &f,
            &router,
            &auth,
            "issues",
            &format!("repo:yard/proj {query}"),
            &[],
        )
        .await;
        assert_eq!(value["total_count"], total, "{query}: {value}");
    }
    // A private board cannot become a membership oracle through positive or negative qualifiers.
    let outsider = token(&f, &router, bob).await;
    for query in ["team-review-requested:yard/hidden", "team:yard/hidden"] {
        let value = search(
            &f,
            &router,
            &outsider,
            "issues",
            &format!("repo:yard/proj {query}"),
            &[],
        )
        .await;
        assert_eq!(value["total_count"], 0);
    }
    let value = search(
        &f,
        &router,
        &outsider,
        "issues",
        &format!("project:alice/{}", project.number),
        &[],
    )
    .await;
    assert_eq!(value["total_count"], 0);
    let value = search(
        &f,
        &router,
        &outsider,
        "issues",
        "repo:alice/proj no:project",
        &[],
    )
    .await;
    assert_eq!(value["total_count"], 2);
    f.finish().await;
}

async fn status(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let org = match db::organizations::create_organization(
        f.pool(),
        alice,
        &db::organizations::NewOrganization::new(
            "yard",
            db::organizations::OrganizationProfile::new(None, None).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    {
        db::organizations::OrganizationOutcome::Applied(org) => org,
        _ => panic!("organization"),
    };
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner {
            actor_id: alice,
            target: db::repositories::creation::RepositoryTarget::Organization(org.id),
        },
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    db::repositories::engagement::watch_repo(f.pool(), alice, repo.id)
        .await
        .unwrap();
    let router = f.router(root.path()).await;
    let auth = token(&f, &router, alice).await;
    let other = token(&f, &router, bob).await;
    let issue = document(
        request(
            &router,
            "POST",
            "/api/v3/repos/yard/proj/issues",
            Some(&other),
            json!({"title":"A new issue","body":"Opening @alice"}),
        )
        .await,
        "issues/create",
        201,
    )
    .await;
    let number = issue["number"].as_i64().unwrap();
    let comment = document(
        request(
            &router,
            "POST",
            &format!("/api/v3/repos/yard/proj/issues/{number}/comments"),
            Some(&other),
            json!({"body":"Latest @alice mention"}),
        )
        .await,
        "issues/create-comment",
        201,
    )
    .await;
    let user = document(
        get(&router, "/api/v3/user", &auth, false).await,
        "users/get-authenticated",
        200,
    )
    .await;
    assert_eq!(user["login"], "alice");
    let path = "/api/v3/notifications?all=true&participating=true&per_page=100";
    let notices = document(
        get(&router, path, &auth, false).await,
        "activity/list-notifications-for-authenticated-user",
        200,
    )
    .await;
    let notice = notices
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["reason"] == "mention")
        .unwrap();
    assert_eq!(notice["repository"]["owner"]["login"], "yard");
    assert_eq!(notice["repository"]["full_name"], "yard/proj");
    assert_eq!(notice["subject"]["title"], "A new issue");
    assert_eq!(notice["subject"]["type"], "Issue");
    assert!(notice["subject"]["url"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/issues/{number}")));
    assert_eq!(notice["subject"]["latest_comment_url"], comment["url"]);
    let latest =
        url::Url::parse(notice["subject"]["latest_comment_url"].as_str().unwrap()).unwrap();
    let body = document(
        get(&router, latest.path(), &auth, false).await,
        "issues/get-comment",
        200,
    )
    .await;
    assert_eq!(body["body"], "Latest @alice mention");
    // gh status fetches read mentions too.
    let thread = url::Url::parse(notice["url"].as_str().unwrap()).unwrap();
    assert_eq!(
        request(&router, "PATCH", thread.path(), Some(&auth), Value::Null)
            .await
            .status(),
        StatusCode::RESET_CONTENT
    );
    let notices = document(
        get(&router, path, &auth, false).await,
        "activity/list-notifications-for-authenticated-user",
        200,
    )
    .await;
    assert_eq!(notices[0]["unread"], false);
    db::events::record_event(
        f.pool(),
        repo.id,
        Some(bob),
        "PullRequestEvent",
        &json!({"action":"opened","pull_request":{"number":123,"title":"A new pull"}}),
        true,
    )
    .await
    .unwrap();
    let events = document(
        get(
            &router,
            "/api/v3/users/alice/received_events?per_page=100",
            &auth,
            false,
        )
        .await,
        "activity/list-received-events-for-user",
        200,
    )
    .await;
    for (kind, title, key, n) in [
        ("IssuesEvent", "A new issue", "issue", number),
        ("PullRequestEvent", "A new pull", "pull_request", 123),
        ("IssueCommentEvent", "A new issue", "issue", number),
    ] {
        let event = events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == kind)
            .unwrap();
        assert_eq!(event["org"]["login"], "yard");
        assert_eq!(event["repo"]["name"], "yard/proj");
        timestamp(&event["created_at"]);
        assert_eq!(event["payload"][key]["title"], title);
        assert_eq!(event["payload"][key]["number"], n);
        assert_eq!(
            event["payload"]["action"],
            if kind == "IssueCommentEvent" {
                "created"
            } else {
                "opened"
            }
        );
        if kind == "IssueCommentEvent" {
            assert_eq!(event["payload"]["comment"]["body"], "Latest @alice mention");
            assert_eq!(event["payload"]["comment"]["html_url"], comment["html_url"]);
        }
    }
    let feed = document(
        get(
            &router,
            "/api/v3/repos/yard/proj/events?per_page=100",
            &auth,
            false,
        )
        .await,
        "activity/list-repo-events",
        200,
    )
    .await;
    assert_eq!(
        feed.as_array().unwrap().len(),
        events.as_array().unwrap().len()
    );
    f.finish().await;
}
#[tokio::test]
async fn gh_rest_search_repositories_content_sqlite() {
    repositories_and_content(false).await;
}
#[tokio::test]
async fn gh_rest_search_repositories_content_postgres() {
    repositories_and_content(true).await;
}
#[tokio::test]
async fn gh_rest_search_discussions_sqlite() {
    discussions(false).await;
}
#[tokio::test]
async fn gh_rest_search_discussions_postgres() {
    discussions(true).await;
}
#[tokio::test]
async fn gh_rest_status_sqlite() {
    status(false).await;
}
#[tokio::test]
async fn gh_rest_status_postgres() {
    status(true).await;
}

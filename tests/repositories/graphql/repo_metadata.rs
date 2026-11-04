//! The remaining RepositoryGraphQL selections emitted by gh repo list --json.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};

const FIELDS: &str = "openGraphImageUrl,usesCustomOpenGraphImage,mirrorUrl,securityPolicyUrl,archivedAt,isBlankIssuesEnabled,isSecurityPolicyEnabled,hasDiscussionsEnabled,diskUsage,isEmpty,isMirror,isUserConfigurationRepository,viewerDefaultCommitEmail,viewerDefaultMergeMethod,viewerHasStarred,viewerPossibleCommitEmails,viewerSubscription,templateRepository{id,name,owner{id,login}},codeOfConduct{key,name,url},contactLinks{about,name,url},fundingLinks{platform,url},licenseInfo{key,name,nickname},languages(first:100){edges{size,node{name}}},primaryLanguage{name},issueTemplates{name,title,body,about},pullRequestTemplates{body,filename},milestones(first:100,states:OPEN){nodes{number,title,description,dueOn}},assignableUsers(first:100){nodes{id,login,name}},mentionableUsers(first:100){nodes{id,login,name}},projects(first:100,states:OPEN){nodes{id,name,number,body,resourcePath}},projectsV2(first:100,query:\"is:open\"){nodes{id,number,title,resourcePath,closed,url}},issues(states:OPEN){totalCount},pullRequests(states:OPEN){totalCount}";

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("metadata-alice").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
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
    let router = create_router(super::patient(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    )));
    let session = Client::login(&router, f.pool(), alice).await;
    let token = tokens::create(
        f.pool(),
        alice,
        Some(&session.session),
        TokenInput {
            name: "repo metadata",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap()
    .value;
    let mut tip = Value::Null;
    for (path, body) in [
        ("main.rs", "fn main() {}\n"),
        ("SECURITY.md", "Report a vulnerability privately.\n"),
        ("CODE_OF_CONDUCT.md", "# Contributor Covenant\n"),
        ("LICENSE", codeyard::backend::repositories::templates::license("MIT").unwrap().text),
        (".github/ISSUE_TEMPLATE/bug.md", "---\nname: Bug\ntitle: Broken\nabout: Report a bug\n---\nSteps\n"),
        (".github/ISSUE_TEMPLATE/config.yml", "blank_issues_enabled: false\ncontact_links:\n  - name: Help\n    about: Ask for help\n    url: https://example.test/help\n"),
        (".github/PULL_REQUEST_TEMPLATE.md", "Explain the change.\n"),
        (".github/FUNDING.yml", "github: [alice, bob]\ncustom: https://example.test/fund\n"),
    ] {
        let (status, saved) = request(&router, Some(&token), "PUT", &format!("/api/v3/repos/metadata-alice/proj/contents/{path}"), json!({"message":"Metadata files","content":STANDARD.encode(body)})).await;
        assert_eq!(status, StatusCode::CREATED, "{saved}");
        tip = saved["commit"]["sha"].clone();
    }
    let data = query(
        &router,
        &token,
        "{repository(owner:\"metadata-alice\",name:\"proj\"){id}}",
        json!({}),
    )
    .await;
    let issue = query(
        &router,
        &token,
        "mutation($input:CreateIssueInput!){createIssue(input:$input){issue{id}}}",
        json!({"input":{"repositoryId":data["repository"]["id"],"title":"Open issue"}}),
    )
    .await;
    assert!(issue["createIssue"]["issue"]["id"].is_string());
    db::pull_requests::records::create_pull_request(
        f.pool(),
        db::pull_requests::records::NewPullRequest {
            issue: None,
            maintainer_can_modify: false,
            repo_id: repo.id,
            head_repo_id: Some(repo.id),
            author_id: alice,
            title: "Open pull",
            body: "",
            head_branch: "feature",
            head_sha: tip.as_str().unwrap(),
            base_branch: "main",
            base_sha: tip.as_str().unwrap(),
            is_draft: false,
        },
    )
    .await
    .unwrap();
    db::milestones::create_milestone(
        f.pool(),
        alice,
        repo.id,
        "Next",
        Some("Ship it"),
        Some("2030-01-01"),
    )
    .await
    .unwrap();
    let project = db::projects::create(
        f.pool(),
        alice,
        &db::users::accounts::AccountCredential::Session(&session.session),
        db::projects::Owner::User(alice),
        "Roadmap",
        None,
        true,
    )
    .await
    .unwrap();
    db::projects::link_repository(
        f.pool(),
        alice,
        &db::users::accounts::AccountCredential::Session(&session.session),
        project.id,
        repo.id,
        true,
    )
    .await
    .unwrap();
    db::repositories::engagement::star_repo(f.pool(), alice, repo.id)
        .await
        .unwrap();
    db::repositories::engagement::set_watch_level(f.pool(), alice, repo.id, "watching", "")
        .await
        .unwrap();
    for text in [
        gh_teams::LIST_OWNER,
        gh_teams::LIST_VIEWER,
        gh_teams::LIST_SEARCH,
    ] {
        let data = query(&router, &token, &text.replace(gh_teams::DEFAULT_FIELDS, FIELDS), json!({"owner":"metadata-alice","perPage":100,"query":"sort:updated-desc fork:true user:@me"})).await;
        let node = if text == gh_teams::LIST_SEARCH {
            &data["search"]["nodes"][0]
        } else {
            &data["repositoryOwner"]["repositories"]["nodes"][0]
        };
        assert_eq!(node.as_object().unwrap().len(), 33, "{node}");
        assert!(node["openGraphImageUrl"]
            .as_str()
            .unwrap()
            .ends_with("/assets/favicon.png"));
        for key in [
            "usesCustomOpenGraphImage",
            "hasDiscussionsEnabled",
            "isEmpty",
            "isMirror",
            "isUserConfigurationRepository",
            "isBlankIssuesEnabled",
        ] {
            assert_eq!(node[key], false, "{key}: {node}");
        }
        for key in ["mirrorUrl", "archivedAt", "templateRepository"] {
            assert_eq!(node[key], Value::Null);
        }
        assert_eq!(node["isSecurityPolicyEnabled"], true);
        assert!(node["securityPolicyUrl"]
            .as_str()
            .unwrap()
            .ends_with("/metadata-alice/proj/blob/refs/heads/main/SECURITY.md"));
        assert!(node["diskUsage"].is_number());
        assert!(node["viewerDefaultCommitEmail"].is_string());
        assert!(node["viewerPossibleCommitEmails"]
            .as_array()
            .unwrap()
            .iter()
            .all(Value::is_string));
        assert_eq!(node["viewerDefaultMergeMethod"], "MERGE");
        assert_eq!(node["viewerHasStarred"], true);
        assert_eq!(node["viewerSubscription"], "SUBSCRIBED");
        assert_eq!(node["codeOfConduct"]["key"], "other");
        assert_eq!(node["codeOfConduct"]["name"], "Other");
        assert!(node["codeOfConduct"]["url"].is_string());
        assert_eq!(
            node["licenseInfo"],
            json!({"key":"mit","name":"MIT","nickname":null})
        );
        assert_eq!(
            node["contactLinks"],
            json!([{"name":"Help","about":"Ask for help","url":"https://example.test/help"}])
        );
        assert_eq!(
            node["fundingLinks"],
            json!([{"platform":"GITHUB","url":"https://github.com/sponsors/alice"},{"platform":"GITHUB","url":"https://github.com/sponsors/bob"},{"platform":"CUSTOM","url":"https://example.test/fund"}])
        );
        assert_eq!(
            node["issueTemplates"],
            json!([{"name":"Bug","title":"Broken","about":"Report a bug","body":"Steps\n"}])
        );
        assert_eq!(
            node["pullRequestTemplates"],
            json!([{"filename":".github/PULL_REQUEST_TEMPLATE.md","body":"Explain the change.\n"}])
        );
        let languages = node["languages"]["edges"].as_array().unwrap();
        assert!(languages
            .iter()
            .all(|l| l["size"].is_number() && l["node"]["name"].is_string()));
        assert!(languages
            .iter()
            .any(|l| l["node"]["name"] == "Rust" && l["size"] == 13));
        assert!(node["primaryLanguage"]["name"].is_string());
        assert_eq!(
            node["milestones"]["nodes"][0],
            json!({"number":1,"title":"Next","description":"Ship it","dueOn":"2030-01-01T00:00:00Z"})
        );
        for key in ["assignableUsers", "mentionableUsers"] {
            let account = &node[key]["nodes"][0];
            assert_eq!(account["login"], "metadata-alice");
            assert!(account["id"].is_string());
            assert!(account["name"].is_null() || account["name"].is_string());
        }
        assert_eq!(node["projects"], json!({"nodes":[]}));
        let project = &node["projectsV2"]["nodes"][0];
        assert!(project["id"].is_string());
        assert_eq!(project["number"], 1);
        assert_eq!(project["title"], "Roadmap");
        assert_eq!(project["closed"], false);
        assert!(project["resourcePath"].is_string());
        assert!(project["url"].is_string());
        assert_eq!(node["issues"], json!({"totalCount":1}));
        assert_eq!(node["pullRequests"], json!({"totalCount":1}));
    }
    // Count-only support must not remove validation from selections that need a page.
    for selection in [
        "issues(states:OPEN){nodes{number}}",
        "pullRequests(states:OPEN){nodes{number}}",
    ] {
        let (_, response) = call(
            &router,
            Some(&token),
            &format!("{{repository(owner:\"metadata-alice\",name:\"proj\"){{{selection}}}}}"),
            json!({}),
        )
        .await;
        assert!(
            response["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("first or last"),
            "{response}"
        );
    }
    f.finish().await;
}

#[tokio::test]
async fn sqlite_gh_repo_list_metadata() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_gh_repo_list_metadata() {
    contract(true).await;
}

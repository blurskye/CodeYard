//! Queries captured from gh 2.102.0, including its fragments and feature detection.
use super::*;
use async_graphql_parser::{
    parse_query,
    types::{ExecutableDocument, Selection, SelectionSet},
};
use codeyard_identity::tokens;

macro_rules! cli {
    ($name:literal) => {
        include_str!(concat!("cli/", $name, ".graphql"))
    };
}

fn selected(data: &Value, set: &SelectionSet, doc: &ExecutableDocument) {
    if data.is_null() {
        return;
    }
    if let Some(rows) = data.as_array() {
        for row in rows {
            selected(row, set, doc);
        }
        return;
    }
    for selection in &set.items {
        match &selection.node {
            Selection::Field(field) => {
                let key = field.node.response_key().node.as_str();
                let value = data
                    .get(key)
                    .unwrap_or_else(|| panic!("Missing {key}: {data}"));
                if !field.node.selection_set.node.items.is_empty() {
                    selected(value, &field.node.selection_set.node, doc);
                }
            }
            Selection::FragmentSpread(spread) => {
                selected(
                    data,
                    &doc.fragments[&spread.node.fragment_name.node]
                        .node
                        .selection_set
                        .node,
                    doc,
                );
            }
            Selection::InlineFragment(fragment) => {
                let applies = fragment
                    .node
                    .type_condition
                    .as_ref()
                    .is_none_or(|condition| {
                        data["__typename"]
                            .as_str()
                            .is_none_or(|ty| ty == condition.node.on.node.as_str())
                    });
                if applies {
                    selected(data, &fragment.node.selection_set.node, doc);
                }
            }
        }
    }
}

async fn exact(router: &Router, token: &str, text: &str, variables: Value) -> Value {
    let data = query(router, token, text, variables).await;
    let doc = parse_query(text).unwrap();
    for (_, operation) in doc.operations.iter() {
        selected(&data, &operation.node.selection_set.node, &doc);
    }
    data
}

async fn contract(pg: bool) {
    use codeyard_db_repos::repositories::protection::{
        self, BranchProtectionRuleUpsert, CommitStatusUpsert,
    };
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("graphql-alice").await;
    let bob = f.actor("graphql-bob").await;
    let reviewer = f.actor("graphql-reviewer").await;
    let root = tempfile::tempdir().unwrap();
    let state = super::patient(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let router = create_router(state.clone());
    let mut credentials = Vec::new();
    for (actor, scope) in [(alice, "write"), (bob, "write"), (alice, "read")] {
        let session = Client::login(&router, f.pool(), actor).await;
        credentials.push(
            tokens::create(
                f.pool(),
                actor,
                Some(&session.session),
                TokenInput {
                    name: "gh-cli",
                    scope,
                    expires_at: None,
                },
            )
            .await
            .unwrap()
            .value,
        );
    }
    let token = &credentials[0];
    let wiki = exact(
        &router,
        token,
        cli!("repo-create"),
        json!({"input":{
            "name":"with-wiki","visibility":"PRIVATE","hasIssuesEnabled":true,"hasWikiEnabled":true,
        }}),
    )
    .await;
    assert!(wiki["createRepository"]["repository"]["id"].is_string());
    let wiki_info = exact(
        &router,
        token,
        cli!("repo-info"),
        json!({"owner":"graphql-alice","name":"with-wiki"}),
    )
    .await;
    assert_eq!(wiki_info["repository"]["hasWikiEnabled"], true);
    let created = exact(
        &router,
        token,
        cli!("repo-create"),
        json!({"input":{
            "name":"proj","visibility":"PRIVATE","hasIssuesEnabled":true,"hasWikiEnabled":false,"template":true,
        }}),
    )
    .await;
    let repo_id = created["createRepository"]["repository"]["id"].clone();
    assert_eq!(
        created["createRepository"]["repository"]["owner"]["login"],
        "graphql-alice"
    );
    let repo =
        codeyard_db_repos::repositories::records::get_repository(f.pool(), "graphql-alice", "proj")
            .await
            .unwrap()
            .unwrap();
    let (status, initial) = request(
        &router,
        Some(token),
        "PUT",
        "/api/v3/repos/graphql-alice/proj/contents/README.md",
        json!({"message":"Initial commit","content":"aGVsbG8K"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{initial}");
    let variables = json!({"owner":"graphql-alice","repo":"proj","name":"proj","pr_number":1});
    for text in [
        cli!("repo-view"),
        cli!("repo-info"),
        cli!("repo-network"),
        cli!("repo-archive-find"),
        cli!("pr-merge-fields"),
        cli!("workflow-fields"),
        cli!("viewer"),
    ] {
        exact(&router, token, text, variables.clone()).await;
    }
    let mut pulls = Vec::new();
    let mut head = Value::Null;
    for branch in ["feature", "second"] {
        let (status, result) = request(
            &router,
            Some(token),
            "POST",
            "/api/v3/repos/graphql-alice/proj/git/refs",
            json!({"ref":format!("refs/heads/{branch}"),"sha":initial["commit"]["sha"]}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{result}");
        let (status, changed) = request(
            &router,
            Some(token),
            "PUT",
            "/api/v3/repos/graphql-alice/proj/contents/change.txt",
            json!({"message":"Feature change","branch":branch,"content":"Y2hhbmdlCg=="}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{changed}");
        let data = exact(&router, token, cli!("pr-create"), json!({"input":{
            "repositoryId":repo_id,"title":format!("Pull {branch}"),"body":"Pull body","headRefName":branch,"baseRefName":"main","draft":true,"maintainerCanModify":true,
        }})).await;
        pulls.push(data["createPullRequest"]["pullRequest"]["id"].clone());
        if branch == "feature" {
            head = changed["commit"]["sha"].clone();
        }
    }
    let pull_id = &pulls[0];
    let input = json!({"input":{"pullRequestId":pull_id}});
    exact(&router, token, cli!("pr-ready-find"), variables.clone()).await;
    let ready = exact(&router, token, cli!("pr-ready"), input.clone()).await;
    assert_eq!(
        ready["markPullRequestReadyForReview"]["pullRequest"]["id"],
        *pull_id
    );
    for (text, vars) in [
        (
            cli!("pr-comment"),
            json!({"input":{"subjectId":pull_id,"body":"A CLI comment"}}),
        ),
        (
            cli!("pr-review"),
            json!({"input":{"pullRequestId":pull_id,"body":"Reviewed","event":"COMMENT"}}),
        ),
    ] {
        exact(&router, token, text, vars).await;
    }
    codeyard_db_repos::labels::create_label(
        f.pool(),
        alice,
        repo.id,
        "bug",
        "ff0000",
        Some("A bug"),
    )
    .await
    .unwrap();
    let labels = exact(&router, token, "query RepositoryLabelList($endCursor:String,$name:String!,$owner:String!){repository(owner: $owner, name: $name){labels(first: 100, orderBy: {field: NAME, direction: ASC}, after: $endCursor){nodes{id,name},pageInfo{hasNextPage,endCursor}}}}", json!({"owner":"graphql-alice","name":"proj"})).await;
    let bug = labels["repository"]["labels"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|label| label["name"] == "bug")
        .unwrap()["id"]
        .clone();
    // gh pr edit --title --add-label sends both mutations at once, and a label change moves the
    // pull request's update time while the title edit is saving.
    for (label_text, title) in [
        ("mutation LabelAdd($input:AddLabelsToLabelableInput!){addLabelsToLabelable(input: $input){__typename}}", "Edited with a label"),
        ("mutation LabelRemove($input:RemoveLabelsFromLabelableInput!){removeLabelsFromLabelable(input: $input){__typename}}", "Edited without a label"),
        ("mutation LabelAdd($input:AddLabelsToLabelableInput!){addLabelsToLabelable(input: $input){__typename}}", "Edited through gh"),
    ] {
        tokio::join!(
            exact(&router, token, cli!("pr-edit"), json!({"input":{"pullRequestId":pull_id,"title":title}})),
            exact(&router, token, label_text, json!({"input":{"labelableId":pull_id,"labelIds":[bug]}})),
        );
    }
    let labeled = codeyard_db_repos::pull_requests::records::get_pull_request(f.pool(), repo.id, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(labeled.title, "Edited through gh");
    assert_eq!(
        codeyard_db_repos::labels::get_pull_request_labels(f.pool(), labeled.repo_id, labeled.id)
            .await
            .unwrap()
            .len(),
        1
    );
    let pull = codeyard_db_repos::pull_requests::records::get_pull_request(f.pool(), repo.id, 1)
        .await
        .unwrap()
        .unwrap();
    codeyard_db_repos::assignees::assign_user(
        f.pool(),
        alice,
        repo.id,
        codeyard_db_repos::assignees::AssignedItem::PullRequest(pull.id),
        alice,
    )
    .await
    .unwrap();
    protection::upsert_commit_status(
        f.pool(),
        CommitStatusUpsert {
            repo_id: repo.id,
            sha: head.as_str().unwrap(),
            context: "cli-check",
            state: "success",
            description: Some("Passed"),
            target_url: Some("https://checks.example.test/one"),
            creator_id: Some(alice),
        },
    )
    .await
    .unwrap();
    protection::upsert_branch_protection_rule(
        f.pool(),
        BranchProtectionRuleUpsert {
            actor_id: alice,
            repo_id: repo.id,
            pattern: "main",
            required_approving_reviews: 2,
            require_status_checks: true,
            required_status_checks: vec!["cli-check".into()],
            require_up_to_date_branch: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for text in [
        cli!("pr-checkout"),
        cli!("pr-merge-find"),
        cli!("pr-close-find"),
        cli!("pr-reopen-find"),
        cli!("pr-review-find"),
        cli!("pr-comment-find"),
        cli!("pr-edit-find"),
        cli!("pr-diff"),
        cli!("pr-checks-find"),
        cli!("pr-update-find"),
    ] {
        let data = exact(&router, token, text, variables.clone()).await;
        assert_eq!(data["repository"]["pullRequest"]["id"], *pull_id);
    }
    let view = exact(&router, token, cli!("pr-view"), variables.clone()).await;
    let view = &view["repository"]["pullRequest"];
    assert_eq!(view["title"], "Edited through gh");
    assert_eq!(view["isDraft"], false);
    assert_eq!(view["commits"]["totalCount"], 1);
    assert_eq!(view["comments"]["totalCount"], 1);
    assert_eq!(view["comments"]["nodes"][0]["body"], "A CLI comment");
    assert_eq!(view["reviews"]["totalCount"], 1);
    assert_eq!(view["assignees"]["nodes"][0]["login"], "graphql-alice");
    assert_eq!(
        view["headRepository"]["nameWithOwner"],
        "graphql-alice/proj"
    );
    let projects = exact(
        &router,
        token,
        cli!("pr-project-items"),
        json!({"owner":"graphql-alice","name":"proj","number":1}),
    )
    .await;
    assert_eq!(
        projects["repository"]["pullRequest"]["projectItems"]["nodes"],
        json!([])
    );
    let listed = exact(
        &router,
        token,
        cli!("pr-list"),
        json!({"owner":"graphql-alice","repo":"proj","limit":30,"state":["OPEN"]}),
    )
    .await;
    assert_eq!(listed["repository"]["pullRequests"]["totalCount"], 2);
    let searched = exact(
        &router,
        token,
        cli!("pr-search"),
        json!({"q":"repo:graphql-alice/proj is:pr is:open","type":"ISSUE","limit":30}),
    )
    .await;
    assert_eq!(searched["search"]["issueCount"], 2);
    let status_variables = json!({"owner":"graphql-alice","repo":"proj","headRefName":"feature",
        "viewerQuery":"repo:graphql-alice/proj state:open is:pr author:graphql-alice",
        "reviewerQuery":"repo:graphql-alice/proj state:open review-requested:graphql-reviewer"});
    let status = exact(&router, token, cli!("pr-status"), status_variables.clone()).await;
    assert_eq!(status["viewerCreated"]["totalCount"], 2);
    assert_eq!(status["reviewRequested"]["totalCount"], 0);
    let current = &status["repository"]["pullRequests"]["edges"][0]["node"];
    assert_eq!(
        current["baseRef"]["branchProtectionRule"]["requiresStrictStatusChecks"],
        true
    );
    assert_eq!(
        current["statusCheckRollup"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]
            ["statusContextCount"],
        1
    );
    let mut numbered_variables = status_variables.clone();
    numbered_variables["number"] = json!(1);
    let numbered = exact(&router, token, cli!("pr-status-number"), numbered_variables).await;
    assert_eq!(
        numbered["repository"]["pullRequest"]["baseRef"]["branchProtectionRule"]
            ["requiredApprovingReviewCount"],
        2
    );
    codeyard_db_repos::repositories::access::change_personal_repository_collaborator(
        f.pool(),
        alice,
        repo.id,
        "graphql-reviewer",
        Some(codeyard_db_core::models::AccessLevel::Write),
    )
    .await
    .unwrap();
    codeyard_db_repos::pull_requests::reviews::request_pull_request_reviewer(
        f.pool(),
        pull.id,
        alice,
        reviewer,
    )
    .await
    .unwrap();
    let status = exact(&router, token, cli!("pr-status"), status_variables).await;
    assert_eq!(status["reviewRequested"]["totalCount"], 1);
    let checks = exact(&router, token, cli!("pr-checks"), json!({"id":pull_id})).await;
    let check = &checks["node"]["statusCheckRollup"]["nodes"][0]["commit"]["statusCheckRollup"]
        ["contexts"]["nodes"][0];
    assert_eq!(check["__typename"], "StatusContext");
    assert_eq!(check["state"], "SUCCESS");
    assert_eq!(check["isRequired"], true);
    let (_, blocked) = call(
        &router,
        Some(token),
        cli!("pr-merge"),
        json!({"input":{"pullRequestId":pull_id,"mergeMethod":"MERGE"}}),
    )
    .await;
    assert!(blocked["errors"].is_array(), "{blocked}");
    assert!(
        !codeyard_db_repos::pull_requests::records::get_pull_request_by_id(f.pool(), pull.id)
            .await
            .unwrap()
            .unwrap()
            .merged
    );
    protection::delete_branch_protection_rule(f.pool(), alice, repo.id, "main")
        .await
        .unwrap();
    let checks = exact(&router, token, cli!("pr-checks"), json!({"id":pull_id})).await;
    assert_eq!(
        checks["node"]["statusCheckRollup"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]
            ["nodes"][0]["isRequired"],
        false
    );
    let comparison = exact(
        &router,
        token,
        cli!("pr-compare"),
        json!({"owner":"graphql-alice","repo":"proj","pullRequestNumber":1,"headRef":"feature"}),
    )
    .await;
    assert_eq!(
        comparison["repository"]["pullRequest"]["baseRef"]["compare"],
        json!({"aheadBy":1,"behindBy":0,"status":"AHEAD"})
    );
    exact(
        &router,
        token,
        cli!("pr-update"),
        json!({"input":{"pullRequestId":pull_id,"expectedHeadOid":head}}),
    )
    .await;
    for (text, field, state) in [
        (cli!("pr-close"), "closePullRequest", "closed"),
        (cli!("pr-reopen"), "reopenPullRequest", "open"),
    ] {
        let data = exact(&router, token, text, input.clone()).await;
        assert_eq!(data[field]["pullRequest"]["id"], *pull_id);
        assert_eq!(
            codeyard_db_repos::pull_requests::records::get_pull_request_by_id(f.pool(), pull.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            state
        );
    }
    for (text, field, locked) in [
        (cli!("pr-lock"), "lockLockable", true),
        (cli!("pr-unlock"), "unlockLockable", false),
    ] {
        let lock_input = if locked {
            json!({"lockableId":pull_id,"lockReason":"SPAM"})
        } else {
            json!({"lockableId":pull_id})
        };
        let data = exact(&router, token, text, json!({"input":lock_input})).await;
        assert_eq!(
            data[field][if locked {
                "lockedRecord"
            } else {
                "unlockedRecord"
            }]["locked"],
            locked
        );
        let read = exact(
            &router,
            token,
            cli!("pr-lock-find"),
            json!({"owner":"graphql-alice","repo":"proj","number":1}),
        )
        .await;
        assert_eq!(read["repository"]["issue"]["locked"], locked);
    }
    let drafted = exact(&router, token, cli!("pr-draft"), input.clone()).await;
    assert_eq!(
        drafted["convertPullRequestToDraft"]["pullRequest"]["id"],
        *pull_id
    );
    assert!(
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(f.pool(), pull.id)
            .await
            .unwrap()
            .unwrap()
            .is_draft
    );
    for credential in [&credentials[1], &credentials[2]] {
        let (_, denied) = call(&router, Some(credential), cli!("pr-ready"), input.clone()).await;
        assert!(denied["errors"].is_array(), "{denied}");
    }
    exact(&router, token, cli!("pr-ready"), input.clone()).await;
    assert!(
        !codeyard_db_repos::pull_requests::records::get_pull_request_by_id(f.pool(), pull.id)
            .await
            .unwrap()
            .unwrap()
            .is_draft
    );
    for (method, headline, body) in [
        (
            "MERGE",
            "Merge pull request #1 from feature",
            "Edited through gh",
        ),
        ("SQUASH", "Edited through gh (#1)", ""),
        ("REBASE", "", ""),
    ] {
        let text = exact(
            &router,
            token,
            cli!("pr-merge-text"),
            json!({"prID":pull_id,"method":method}),
        )
        .await;
        assert_eq!(text["node"]["viewerMergeHeadlineText"], headline);
        assert_eq!(text["node"]["viewerMergeBodyText"], body);
    }
    let (_, invalid) = call(
        &router,
        Some(token),
        cli!("pr-merge"),
        json!({"input":{
            "pullRequestId":pull_id,"mergeMethod":"MERGE","authorEmail":"bad\n@example.test",
        }}),
    )
    .await;
    assert!(invalid["errors"].is_array(), "{invalid}");
    let (_, unmerged) = call(&router, Some(token), cli!("pr-revert"), input.clone()).await;
    assert!(unmerged["errors"].is_array(), "{unmerged}");
    let mut cursor = Value::Null;
    let mut seen = BTreeSet::new();
    loop {
        let page = exact(
            &router,
            token,
            cli!("paginate"),
            json!({"endCursor":cursor}),
        )
        .await;
        let page = &page["repository"]["pullRequests"];
        assert_eq!(page["nodes"].as_array().unwrap().len(), 1);
        assert!(seen.insert(page["nodes"][0]["number"].as_i64().unwrap()));
        if page["pageInfo"]["hasNextPage"] == false {
            break;
        }
        let next = page["pageInfo"]["endCursor"].clone();
        assert!(next.is_string());
        assert_ne!(next, cursor);
        cursor = next;
    }
    assert_eq!(seen, BTreeSet::from([1, 2]));
    for credential in [&credentials[1], &credentials[2]] {
        let (_, denied) = call(
            &router,
            Some(credential),
            cli!("pr-merge"),
            json!({"input":{"pullRequestId":pull_id,"mergeMethod":"MERGE"}}),
        )
        .await;
        assert!(denied["errors"].is_array(), "{denied}");
    }
    let (_, hidden) = call(&router, Some(&credentials[1]), cli!("pr-view"), variables).await;
    assert_eq!(hidden["errors"][0]["type"], "NOT_FOUND");
    Box::pin(merge_and_revert(
        router.clone(),
        &state,
        &credentials,
        &repo,
        pull_id,
        &initial,
        &pull,
    ))
    .await;
    Box::pin(clone_templates(
        router.clone(),
        &f,
        &credentials,
        &repo,
        &repo_id,
    ))
    .await;
    let archived = exact(
        &router,
        token,
        cli!("repo-archive"),
        json!({"input":{"repositoryId":repo_id}}),
    )
    .await;
    assert_eq!(archived["archiveRepository"]["repository"]["id"], repo_id);
    assert!(
        codeyard_db_repos::repositories::records::get_repository_by_id(f.pool(), repo.id)
            .await
            .unwrap()
            .unwrap()
            .is_archived
    );
}

async fn merge_and_revert(
    router: Router,
    state: &AppState,
    credentials: &[String],
    repo: &codeyard_db_core::models::Repository,
    pull_id: &Value,
    initial: &Value,
    pull: &codeyard_db_core::models::PullRequest,
) {
    let token = &credentials[0];
    let input = json!({"input":{"pullRequestId":pull_id}});
    exact(
        &router,
        token,
        cli!("pr-merge"),
        json!({"input":{"pullRequestId":pull_id,"mergeMethod":"MERGE","authorEmail":"chosen@example.test",
            "commitHeadline":"CLI merge headline","commitBody":"CLI merge body"}}),
    )
    .await;
    let merged =
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(&state.pool, pull.id)
            .await
            .unwrap()
            .unwrap();
    let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
    let commit = git
        .find_commit(merged.merge_commit_sha.as_ref().unwrap().parse().unwrap())
        .unwrap();
    assert_eq!(commit.author().email(), Some("chosen@example.test"));
    assert_ne!(commit.committer().email(), Some("chosen@example.test"));
    assert_eq!(
        commit.message(),
        Some("CLI merge headline\n\nCLI merge body")
    );
    assert!(
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(&state.pool, pull.id)
            .await
            .unwrap()
            .unwrap()
            .merged
    );
    for credential in [&credentials[1], &credentials[2]] {
        let (_, denied) = call(&router, Some(credential), cli!("pr-revert"), input.clone()).await;
        assert!(denied["errors"].is_array(), "{denied}");
    }
    let reverted = exact(
        &router,
        token,
        cli!("pr-revert"),
        json!({"input":{
            "pullRequestId":pull_id,"title":"Undo CLI change","body":"Revert body","draft":true,
        }}),
    )
    .await;
    assert_eq!(reverted["revertPullRequest"]["pullRequest"]["id"], *pull_id);
    let revert = &reverted["revertPullRequest"]["revertPullRequest"];
    assert_eq!(revert["number"], 3);
    assert!(revert["url"].as_str().unwrap().ends_with("/pull/3"));
    let stored =
        codeyard_db_repos::pull_requests::records::get_pull_request(&state.pool, repo.id, 3)
            .await
            .unwrap()
            .unwrap();
    assert!(stored.is_draft);
    assert_eq!(stored.title, "Undo CLI change");
    assert_eq!(stored.body, "Revert body");
    let undo = git.find_commit(stored.head_sha.parse().unwrap()).unwrap();
    assert_eq!(undo.parent_id(0).unwrap(), commit.id());
    let initial_commit = git
        .find_commit(initial["commit"]["sha"].as_str().unwrap().parse().unwrap())
        .unwrap();
    assert_eq!(undo.tree_id(), initial_commit.tree_id());
    exact(
        &router,
        token,
        cli!("pr-ready"),
        json!({"input":{"pullRequestId":revert["id"]}}),
    )
    .await;
    // `gh pr merge --auto` waits for the background check, which merges through the
    // ordinary merge path once the requirements pass.
    let view_revert = json!({"owner":"graphql-alice","repo":"proj","pr_number":3});
    exact(
        &router,
        token,
        cli!("pr-merge-auto"),
        json!({"input":{"pullRequestId":revert["id"],"mergeMethod":"SQUASH",
            "commitHeadline":"Auto headline","expectedHeadOid":stored.head_sha}}),
    )
    .await;
    let viewed = exact(&router, token, cli!("pr-view"), view_revert.clone()).await;
    let auto = &viewed["repository"]["pullRequest"]["autoMergeRequest"];
    assert_eq!(auto["mergeMethod"], "SQUASH");
    assert_eq!(auto["commitHeadline"], "Auto headline");
    assert_eq!(auto["enabledBy"]["login"], "graphql-alice");
    assert!(auto["enabledAt"].is_string());
    for credential in [&credentials[1], &credentials[2]] {
        let (_, denied) = call(
            &router,
            Some(credential),
            cli!("pr-merge-auto-disable"),
            json!({"input":{"pullRequestId":revert["id"]}}),
        )
        .await;
        assert!(denied["errors"].is_array(), "{denied}");
    }
    exact(
        &router,
        token,
        cli!("pr-merge-auto-disable"),
        json!({"input":{"pullRequestId":revert["id"]}}),
    )
    .await;
    let viewed = exact(&router, token, cli!("pr-view"), view_revert).await;
    assert!(viewed["repository"]["pullRequest"]["autoMergeRequest"].is_null());
    exact(
        &router,
        token,
        cli!("pr-merge-auto"),
        json!({"input":{"pullRequestId":revert["id"],"mergeMethod":"MERGE"}}),
    )
    .await;
    codeyard_repos_pulls::repositories::pull_requests::auto_merge::run_due(state)
        .await
        .unwrap();
    assert!(
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(&state.pool, stored.id)
            .await
            .unwrap()
            .unwrap()
            .merged
    );
    assert!(
        codeyard_db_repos::pull_requests::auto_merge::get_auto_merge(&state.pool, stored.id)
            .await
            .unwrap()
            .is_none()
    );
    let (_, already_reverted) = call(&router, Some(token), cli!("pr-revert"), input.clone()).await;
    assert!(already_reverted["errors"].is_array(), "{already_reverted}");
}

async fn clone_templates(
    router: Router,
    f: &Database,
    credentials: &[String],
    repo: &codeyard_db_core::models::Repository,
    repo_id: &Value,
) {
    let token = &credentials[0];
    let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
    let owner_id = exact(&router, token, cli!("viewer"), json!({})).await;
    let owner_node = exact(&router, token, cli!("viewer-id"), json!({})).await;
    assert_eq!(owner_id["viewer"]["login"], "graphql-alice");
    let not_template = exact(
        &router,
        token,
        cli!("repo-info"),
        json!({"owner":"graphql-alice","name":"with-wiki"}),
    )
    .await;
    let (_, denied) = call(
        &router,
        Some(token),
        cli!("repo-template"),
        json!({"input":{
            "repositoryId":not_template["repository"]["id"],"ownerId":owner_node["viewer"]["id"],
            "name":"not-a-template","visibility":"PRIVATE",
        }}),
    )
    .await;
    assert!(denied["errors"].is_array(), "{denied}");
    assert!(codeyard_db_repos::repositories::records::get_repository(
        f.pool(),
        "graphql-alice",
        "not-a-template"
    )
    .await
    .unwrap()
    .is_none());
    for (name, all_branches) in [("from-template", false), ("from-template-all", true)] {
        let cloned = exact(&router, token, cli!("repo-template"), json!({"input":{
            "repositoryId":repo_id,"ownerId":owner_node["viewer"]["id"],"name":name,
            "visibility":"PRIVATE","description":"Cloned template","includeAllBranches":all_branches,
        }})).await;
        assert_eq!(
            cloned["cloneTemplateRepository"]["repository"]["name"],
            name
        );
        assert_eq!(
            cloned["cloneTemplateRepository"]["repository"]["owner"]["login"],
            "graphql-alice"
        );
        let cloned = codeyard_db_repos::repositories::records::get_repository(
            f.pool(),
            "graphql-alice",
            name,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!cloned.is_fork);
        assert!(cloned.forked_from_id.is_none());
        let clone_git = git2::Repository::open_bare(cloned.disk_path).unwrap();
        let branches: Vec<_> = clone_git
            .branches(Some(git2::BranchType::Local))
            .unwrap()
            .collect();
        assert_eq!(branches.len(), if all_branches { 4 } else { 1 });
        let source_main = git
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let clone_main = clone_git
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        assert_eq!(clone_main.parent_count(), 0);
        assert_eq!(clone_main.tree_id(), source_main.tree_id());
        assert!(clone_git.find_commit(source_main.id()).is_err());
    }
    for credential in [&credentials[1], &credentials[2]] {
        let (_, denied) = call(&router, Some(credential), cli!("repo-template"), json!({"input":{
            "repositoryId":repo_id,"ownerId":owner_node["viewer"]["id"],"name":"denied-template","visibility":"PRIVATE",
        }})).await;
        assert!(denied["errors"].is_array(), "{denied}");
    }
    assert!(codeyard_db_repos::repositories::records::get_repository(
        f.pool(),
        "graphql-alice",
        "denied-template"
    )
    .await
    .unwrap()
    .is_none());
}

#[tokio::test]
async fn sqlite_gh_cli_queries() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_gh_cli_queries() {
    contract(true).await;
}

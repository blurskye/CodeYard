//! CLI 2.102.0 query text from api/queries_issue.go, query_builder.go and pkg/cmd/issue.
use super::*;

// The two substitutions are IssueGraphQL and PullRequestGraphQL, respectively.
const BY_NUMBER: &str = r#"
	query IssueByNumber($owner: String!, $repo: String!, $number: Int!) {
		repository(owner: $owner, name: $repo) {
			hasIssuesEnabled
			issue: issueOrPullRequest(number: $number) {
				__typename
				...on Issue{%[1]s}
				...on PullRequest{%[2]s}
			}
		}
	}"#;
const LIST: &str = r#"
	query IssueList($owner: String!, $repo: String!, $limit: Int, $endCursor: String, $states: [IssueState!] = OPEN, $assignee: String, $author: String, $mention: String) {
		repository(owner: $owner, name: $repo) {
			hasIssuesEnabled
			issues(first: $limit, after: $endCursor, orderBy: {field: CREATED_AT, direction: DESC}, states: $states, filterBy: {assignee: $assignee, createdBy: $author, mentioned: $mention}) {
				totalCount
				nodes {
					...issue
				}
				pageInfo {
					hasNextPage
					endCursor
				}
			}
		}
	}
	"#;
const SEARCH: &str = r#"query IssueSearch($repo: String!, $owner: String!, $type: SearchType!, $limit: Int, $after: String, $query: String!) {
			repository(name: $repo, owner: $owner) {
				hasIssuesEnabled
			}
			search(type: $type, last: $limit, after: $after, query: $query) {
				issueCount
				nodes { ...issue }
				pageInfo {
					hasNextPage
					endCursor
				}
			}
		}"#;
const STATUS: &str = r#"
	query IssueStatus($owner: String!, $repo: String!, $viewer: String!, $per_page: Int = 10) {
		repository(owner: $owner, name: $repo) {
			hasIssuesEnabled
			assigned: issues(filterBy: {assignee: $viewer, states: OPEN}, first: $per_page, orderBy: {field: UPDATED_AT, direction: DESC}) {
				totalCount
				nodes {
					...issue
				}
			}
			mentioned: issues(filterBy: {mentioned: $viewer, states: OPEN}, first: $per_page, orderBy: {field: UPDATED_AT, direction: DESC}) {
				totalCount
				nodes {
					...issue
				}
			}
			authored: issues(filterBy: {createdBy: $viewer, states: OPEN}, first: $per_page, orderBy: {field: UPDATED_AT, direction: DESC}) {
				totalCount
				nodes {
					...issue
				}
			}
		}
	}"#;
const CREATE: &str = r#"
	mutation IssueCreate($input: CreateIssueInput!) {
		createIssue(input: $input) {
			issue {
				id
				url
			}
		}
	}"#;
const REPOSITORY: &str = r#"
	query IssueRepositoryInfo($owner: String!, $name: String!) {
		repository(owner: $owner, name: $name) {
			id
			databaseId
			name
			owner { login }
			hasIssuesEnabled
			viewerPermission
		}
	}"#;
const ISSUE_ID: &str = r#"
	query IssueNodeID($owner: String!, $name: String!, $number: Int!) {
		repository(owner: $owner, name: $name) {
			issue(number: $number) {
				id
			}
		}
	}"#;
const TYPES: &str = r#"
	query RepositoryIssueTypes($owner: String!, $name: String!) {
		repository(owner: $owner, name: $name) {
			issueTypes(first: 50) {
				nodes { id, name, description, color }
			}
		}
	}"#;
const LABELS: &str = r#"
	query LabelList($owner: String!, $repo: String!, $limit: Int!, $endCursor: String, $query: String, $orderBy: LabelOrder) {
		repository(owner: $owner, name: $repo) {
			labels(first: $limit, after: $endCursor, query: $query, orderBy: $orderBy) {
				totalCount,
				nodes {
					...label
				}
				pageInfo {
					hasNextPage
					endCursor
				}
			}
		}
	}"#;
const COMMENTS: &str = "comments(first: 100) {nodes {id,author{login,...on User{id,name}},authorAssociation,body,createdAt,includesCreatedEdit,isMinimized,minimizedReason,reactionGroups{content,users{totalCount}},url,viewerDidAuthor},pageInfo{hasNextPage,endCursor},totalCount}";
const LAST_COMMENT: &str = "comments(last: 1) {nodes {author{login,...on User{id,name}},authorAssociation,body,createdAt,includesCreatedEdit,isMinimized,minimizedReason,reactionGroups{content,users{totalCount}}},totalCount}";

fn lookup(issue: &str, pull: &str) -> String {
    BY_NUMBER.replace("%[1]s", issue).replace("%[2]s", pull)
}
fn keys(value: &Value, names: &[&str]) {
    assert_eq!(value.as_object().unwrap().len(), names.len(), "{value}");
    for name in names {
        assert!(value.get(name).is_some(), "Missing {name}: {value}");
    }
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("gh-issues-alice").await;
    let bob = f.actor("gh-issues-bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        true,
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
    let mut credentials = Vec::new();
    for (actor, scope) in [(alice, "write"), (bob, "write"), (alice, "read")] {
        let session = Client::login(&router, f.pool(), actor).await;
        credentials.push(
            tokens::create(
                f.pool(),
                actor,
                Some(&session.session),
                TokenInput {
                    name: "gh-issues",
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
    let templates = "query IssueTemplates($name:String!,$owner:String!){repository(owner: $owner, name: $name){issueTemplates{name,body,title}}}";
    let template_vars = json!({"owner":"gh-issues-alice","name":"proj"});
    assert_eq!(
        query(&router, token, templates, template_vars.clone()).await["repository"]
            ["issueTemplates"],
        json!([])
    );
    use base64::Engine;
    let (status, saved) = request(&router, Some(token), "PUT", "/api/v3/repos/gh-issues-alice/proj/contents/.github/ISSUE_TEMPLATE/bug.md", json!({
        "message":"Add issue template",
        "content":base64::engine::general_purpose::STANDARD.encode("---\nname: Bug report\nabout: Report a bug\ntitle: A bug\n---\nDescribe the bug.\n")
    })).await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    assert_eq!(
        query(&router, token, templates, template_vars).await["repository"]["issueTemplates"],
        json!([{"name":"Bug report","body":"Describe the bug.\n","title":"A bug"}])
    );
    let branch = "query FindRepoBranchID($name:String!$owner:String!$ref:String!){repository(owner: $owner, name: $name){id,defaultBranchRef{target{oid}},ref(qualifiedName: $ref){target{oid}}}}";
    let data = query(&router, token, branch, json!({"name":"proj","owner":"gh-issues-alice","ref":format!("refs/heads/{}",repo.default_branch)})).await;
    keys(&data["repository"], &["id", "defaultBranchRef", "ref"]);
    assert_eq!(
        data["repository"]["ref"]["target"]["oid"],
        saved["commit"]["sha"]
    );
    assert_eq!(
        data["repository"]["defaultBranchRef"]["target"]["oid"],
        saved["commit"]["sha"]
    );
    assert!(query(
        &router,
        token,
        branch,
        json!({"name":"proj","owner":"gh-issues-alice","ref":"refs/heads/missing"})
    )
    .await["repository"]["ref"]
        .is_null());
    let vars =
        json!({"owner":"gh-issues-alice","repo":"proj","name":"proj","number":1,"limit":100});
    let data = query(&router, token, REPOSITORY, vars.clone()).await;
    keys(
        &data["repository"],
        &[
            "id",
            "databaseId",
            "name",
            "owner",
            "hasIssuesEnabled",
            "viewerPermission",
        ],
    );
    assert_eq!(data["repository"]["viewerPermission"], "ADMIN");
    let repo_id = data["repository"]["id"].clone();
    let data = query(
        &router,
        token,
        "query UserCurrent{viewer{login}}",
        json!({}),
    )
    .await;
    assert_eq!(data, json!({"viewer":{"login":"gh-issues-alice"}}));
    let data = query(
        &router,
        token,
        CREATE,
        json!({"input":{"repositoryId":repo_id,"title":"Parent","body":"Hello @gh-issues-alice"}}),
    )
    .await;
    let parent = data["createIssue"]["issue"]["id"].clone();
    keys(&data["createIssue"]["issue"], &["id", "url"]);
    assert!(url::Url::parse(data["createIssue"]["issue"]["url"].as_str().unwrap()).is_ok());
    let data = query(
        &router,
        token,
        CREATE,
        json!({"input":{"repositoryId":repo_id,"title":"Child","body":"Second"}}),
    )
    .await;
    let child = data["createIssue"]["issue"]["id"].clone();
    assert_eq!(
        query(&router, token, ISSUE_ID, vars.clone()).await["repository"]["issue"]["id"],
        parent
    );
    let parent_record = db::issues::get_issue(f.pool(), repo.id, 1)
        .await
        .unwrap()
        .unwrap();
    let child_record = db::issues::get_issue(f.pool(), repo.id, 2)
        .await
        .unwrap()
        .unwrap();
    let kind = db::issue_attributes::configure(
        f.pool(),
        repo.id,
        alice,
        &db::issue_attributes::Definition {
            id: None,
            expected_updated: None,
            kind: "type".into(),
            name: "Task".into(),
            description: Some("A task".into()),
            data_type: None,
            visibility: "all".into(),
            is_enabled: true,
            color: Some("blue".into()),
            options: vec![],
        },
    )
    .await
    .unwrap();
    let data = query(&router, token, TYPES, vars.clone()).await;
    let kind_id = data["repository"]["issueTypes"]["nodes"][0]["id"].clone();
    keys(
        &data["repository"]["issueTypes"]["nodes"][0],
        &["id", "name", "description", "color"],
    );
    assert_eq!(
        data["repository"]["issueTypes"]["nodes"][0]["color"],
        "BLUE"
    );
    let mutation = "mutation UpdateIssueIssueType($input:UpdateIssueIssueTypeInput!){updateIssueIssueType(input: $input){issue{id}}}";
    assert_eq!(
        query(
            &router,
            token,
            mutation,
            json!({"input":{"issueId":parent,"issueTypeId":kind_id}})
        )
        .await["updateIssueIssueType"]["issue"]["id"],
        parent
    );
    assert_eq!(
        db::issue_attributes::assigned_type(f.pool(), repo.id, parent_record.id)
            .await
            .unwrap()
            .unwrap()
            .id,
        kind
    );
    for (name, ty, field, input) in [
        (
            "AddSubIssue",
            "AddSubIssueInput",
            "addSubIssue",
            json!({"issueId":parent,"subIssueId":child,"replaceParent":true}),
        ),
        (
            "AddBlockedBy",
            "AddBlockedByInput",
            "addBlockedBy",
            json!({"issueId":parent,"blockingIssueId":child}),
        ),
    ] {
        let text =
            format!("mutation {name}($input:{ty}!){{{field}(input: $input){{issue{{id}}}}}}");
        assert_eq!(
            query(&router, token, &text, json!({"input":input})).await[field]["issue"]["id"],
            parent
        );
    }
    let label = db::labels::create_label(f.pool(), alice, repo.id, "bug", "ff0000", Some("A bug"))
        .await
        .unwrap();
    db::labels::create_label(f.pool(), alice, repo.id, "other", "00ff00", None)
        .await
        .unwrap();
    let labels = format!("fragment label on Label{{color,createdAt,description,id,isDefault,name,updatedAt,url}}{LABELS}");
    let mut label_vars = vars.clone();
    label_vars["limit"] = json!(1);
    label_vars["orderBy"] = json!({"field":"NAME","direction":"ASC"});
    let data = query(&router, token, &labels, label_vars.clone()).await;
    let connection = &data["repository"]["labels"];
    assert_eq!(connection["totalCount"], 2);
    assert_eq!(connection["pageInfo"]["hasNextPage"], true);
    let label_id = connection["nodes"][0]["id"].clone();
    keys(
        &connection["nodes"][0],
        &[
            "color",
            "createdAt",
            "description",
            "id",
            "isDefault",
            "name",
            "updatedAt",
            "url",
        ],
    );
    assert_eq!(connection["nodes"][0]["isDefault"], false);
    assert_eq!(
        connection["nodes"][0]["createdAt"],
        connection["nodes"][0]["updatedAt"]
    );
    assert!(url::Url::parse(connection["nodes"][0]["url"].as_str().unwrap()).is_ok());
    label_vars["endCursor"] = connection["pageInfo"]["endCursor"].clone();
    assert_eq!(
        query(&router, token, &labels, label_vars).await["repository"]["labels"]["nodes"][0]
            ["name"],
        "other"
    );
    db::labels::update_label(
        f.pool(),
        alice,
        repo.id,
        label.id,
        None,
        None,
        Some(Some("Changed")),
    )
    .await
    .unwrap();
    let data = query(&router,token,&labels,json!({"owner":"gh-issues-alice","repo":"proj","limit":100,"query":"Changed","orderBy":{"field":"CREATED_AT","direction":"DESC"}})).await;
    assert_eq!(data["repository"]["labels"]["totalCount"], 1);
    assert_eq!(
        data["repository"]["labels"]["nodes"][0]["description"],
        "Changed"
    );
    let add_label = "mutation LabelAdd($input:AddLabelsToLabelableInput!){addLabelsToLabelable(input: $input){__typename}}";
    assert_eq!(
        query(
            &router,
            token,
            add_label,
            json!({"input":{"labelableId":parent,"labelIds":[label_id]}})
        )
        .await,
        json!({"addLabelsToLabelable":{"__typename":"AddLabelsToLabelablePayload"}})
    );
    db::assignees::assign_user(
        f.pool(),
        alice,
        repo.id,
        db::assignees::AssignedItem::Issue(parent_record.id),
        alice,
    )
    .await
    .unwrap();
    let milestone = db::milestones::create_milestone(
        f.pool(),
        alice,
        repo.id,
        "Release",
        Some("Milestone body"),
        None,
    )
    .await
    .unwrap();
    db::milestones::set_issue_milestone(
        f.pool(),
        alice,
        repo.id,
        parent_record.id,
        Some(milestone.number),
    )
    .await
    .unwrap();
    for (text,field,names) in [
        ("query RepositoryLabelList($endCursor:String,$name:String!,$owner:String!){repository(owner: $owner, name: $name){labels(first: 100, orderBy: {field: NAME, direction: ASC}, after: $endCursor){nodes{id,name},pageInfo{hasNextPage,endCursor}}}}","labels",vec!["id","name"]),
        ("query RepositoryMilestoneList($endCursor:String,$name:String!,$owner:String!,$states:[MilestoneState!]!){repository(owner: $owner, name: $name){milestones(states: $states, first: 100, after: $endCursor){nodes{id,title},pageInfo{hasNextPage,endCursor}}}}","milestones",vec!["id","title"]),
        ("query RepositoryAssignableUsers($endCursor:String,$name:String!,$owner:String!){repository(owner: $owner, name: $name){assignableUsers(first: 100, after: $endCursor){nodes{id,login,name},pageInfo{hasNextPage,endCursor}}}}","assignableUsers",vec!["id","login","name"]),
    ] {
        let mut vars = vars.clone(); vars["states"] = json!(["OPEN"]);
        let data = query(&router,token,text,vars).await;
        keys(&data["repository"][field]["nodes"][0],&names);
        keys(&data["repository"][field]["pageInfo"],&["hasNextPage","endCursor"]);
    }
    let data = query(&router,token,"query RepositoryMilestoneByNumber($name:String!,$number:Int!,$owner:String!){repository(owner: $owner, name: $name){milestone(number: $number){id,title}}}",vars.clone()).await;
    assert_eq!(data["repository"]["milestone"]["title"], "Release");
    for (text,key,member) in [
        ("query Issue_fields{Issue: __type(name: \"Issue\"){fields(includeDeprecated: true){name}}}","Issue","subIssues"),
        ("query Repository_fields{Repository: __type(name: \"Repository\"){fields(includeDeprecated: true){name}}}","Repository","issueTypes"),
        ("query SearchType_enumValues{SearchType: __type(name: \"SearchType\"){enumValues(includeDeprecated: true){name}}}","SearchType","ISSUE_ADVANCED"),
        ("query LinkedBranchFeature{LinkedBranch: __type(name: \"LinkedBranch\"){fields{name}}}","LinkedBranch","ref"),
    ] {
        let data = query(&router,token,text,json!({})).await;
        let values = if key=="SearchType" {"enumValues"} else {"fields"};
        assert!(data[key][values].as_array().unwrap().iter().any(|v|v["name"]==member));
    }
    let create_comment = "mutation CommentCreate($input:AddCommentInput!){addComment(input: $input){commentEdge{node{url}}}}";
    let data = query(
        &router,
        token,
        create_comment,
        json!({"input":{"subjectId":parent,"body":"A comment"}}),
    )
    .await;
    // The same comment link as the REST html_url: the page and anchor the issue page renders.
    let url = data["addComment"]["commentEdge"]["node"]["url"]
        .as_str()
        .unwrap();
    assert!(
        url.contains("?comment=") && url.contains("#comment-"),
        "{url}"
    );
    let fields = format!("id,number,title,body,state,closed,createdAt,updatedAt,closedAt,url,author{{login,...on User{{id,name}}}},assignees(first:100){{nodes{{id,login,name,databaseId}},totalCount}},labels(first:100){{nodes{{id,name,description,color}},totalCount}},milestone{{number,title,description,dueOn}},reactionGroups{{content,users{{totalCount}}}},{COMMENTS}");
    let issue_fields = format!("{fields},stateReason,issueType{{id,name,description,color}},parent{{id,number,title,url,state,repository{{nameWithOwner}}}},subIssues(first:100){{nodes{{id,number,title,url,state,repository{{nameWithOwner}}}},totalCount}},subIssuesSummary{{total,completed,percentCompleted}},blockedBy(first:50){{nodes{{id,number,title,url,state,repository{{nameWithOwner}}}},totalCount}},blocking(first:50){{nodes{{id,number,title,url,state,repository{{nameWithOwner}}}},totalCount}}");
    let data = query(
        &router,
        token,
        &lookup(&issue_fields, &fields),
        vars.clone(),
    )
    .await;
    let issue = &data["repository"]["issue"];
    keys(
        issue,
        &[
            "__typename",
            "id",
            "number",
            "title",
            "body",
            "state",
            "closed",
            "createdAt",
            "updatedAt",
            "closedAt",
            "url",
            "author",
            "assignees",
            "labels",
            "milestone",
            "reactionGroups",
            "comments",
            "stateReason",
            "issueType",
            "parent",
            "subIssues",
            "subIssuesSummary",
            "blockedBy",
            "blocking",
        ],
    );
    assert_eq!(issue["__typename"], "Issue");
    assert_eq!(issue["assignees"]["nodes"][0]["login"], "gh-issues-alice");
    assert_eq!(issue["labels"]["nodes"][0]["name"], "bug");
    assert_eq!(
        issue["milestone"],
        json!({"number":milestone.number,"title":"Release","description":"Milestone body","dueOn":null})
    );
    assert_eq!(issue["subIssues"]["nodes"][0]["id"], child);
    assert_eq!(
        issue["subIssuesSummary"],
        json!({"total":1,"completed":0,"percentCompleted":0})
    );
    assert_eq!(issue["blockedBy"]["totalCount"], 1);
    let comment = &issue["comments"]["nodes"][0];
    keys(
        comment,
        &[
            "id",
            "author",
            "authorAssociation",
            "body",
            "createdAt",
            "includesCreatedEdit",
            "isMinimized",
            "minimizedReason",
            "reactionGroups",
            "url",
            "viewerDidAuthor",
        ],
    );
    assert_eq!(comment["body"], "A comment");
    assert_eq!(comment["authorAssociation"], "OWNER");
    assert_eq!(comment["viewerDidAuthor"], true);
    assert_eq!(comment["reactionGroups"].as_array().unwrap().len(), 8);
    let comment_id = comment["id"].clone();
    let page_comments = "query CommentsForIssue($endCursor:String,$id:ID!){node(id: $id){...on Issue{comments(first: 100, after: $endCursor){nodes{id,author{login},authorAssociation,body,createdAt,includesCreatedEdit,isMinimized,minimizedReason,reactionGroups{content,users{totalCount}},url,viewerDidAuthor},totalCount,pageInfo{hasNextPage,endCursor}}},...on PullRequest{comments(first: 100, after: $endCursor){nodes{id,author{login},authorAssociation,body,createdAt,includesCreatedEdit,isMinimized,minimizedReason,reactionGroups{content,users{totalCount}},url,viewerDidAuthor},totalCount,pageInfo{hasNextPage,endCursor}}}}}";
    let page = query(&router, token, page_comments, json!({"id":parent})).await;
    assert_eq!(page["node"]["comments"]["totalCount"], 1);
    assert_eq!(page["node"]["comments"]["nodes"][0]["id"], comment_id);
    let text = "mutation CommentUpdate($input:UpdateIssueCommentInput!){updateIssueComment(input: $input){issueComment{url}}}";
    assert!(query(
        &router,
        token,
        text,
        json!({"input":{"id":comment_id,"body":"Edited"}})
    )
    .await["updateIssueComment"]["issueComment"]["url"]
        .is_string());
    let data = query(
        &router,
        token,
        &lookup(LAST_COMMENT, LAST_COMMENT),
        vars.clone(),
    )
    .await;
    assert_eq!(
        data["repository"]["issue"]["comments"]["nodes"][0]["body"],
        "Edited"
    );
    assert_eq!(
        data["repository"]["issue"]["comments"]["nodes"][0]["includesCreatedEdit"],
        true
    );
    let text =
        "mutation IssueUpdate($input:UpdateIssueInput!){updateIssue(input: $input){__typename}}";
    assert_eq!(
        query(
            &router,
            token,
            text,
            json!({"input":{"id":parent,"title":"Renamed","body":"Hello @gh-issues-alice"}})
        )
        .await,
        json!({"updateIssue":{"__typename":"UpdateIssuePayload"}})
    );
    let list_fields = "number,title,url,state,updatedAt,labels(first:100){nodes{id,name,description,color},totalCount},stateReason";
    let list_query = format!("fragment issue on Issue {{{list_fields}}}{LIST}");
    let mut list_vars = vars.clone();
    list_vars["limit"] = json!(1);
    let data = query(&router, token, &list_query, list_vars.clone()).await;
    assert_eq!(data["repository"]["issues"]["totalCount"], 2);
    assert_eq!(data["repository"]["issues"]["nodes"][0]["number"], 2);
    list_vars["endCursor"] = data["repository"]["issues"]["pageInfo"]["endCursor"].clone();
    assert_eq!(
        query(&router, token, &list_query, list_vars).await["repository"]["issues"]["nodes"][0]
            ["number"],
        1
    );
    for filter in ["author", "assignee", "mention"] {
        let mut vars = vars.clone();
        vars[filter] = json!("gh-issues-alice");
        assert_eq!(
            query(&router, token, &list_query, vars).await["repository"]["issues"]["totalCount"],
            if filter == "author" { 2 } else { 1 }
        );
    }
    let data = query(&router,token,&format!("fragment issue on Issue{{number,title,url,state,updatedAt,labels(first:100){{nodes{{id,name,description,color}},totalCount}}}}{STATUS}"),json!({"owner":"gh-issues-alice","repo":"proj","viewer":"gh-issues-alice"})).await;
    assert_eq!(data["repository"]["assigned"]["totalCount"], 1);
    assert_eq!(data["repository"]["mentioned"]["totalCount"], 1);
    assert_eq!(data["repository"]["authored"]["totalCount"], 2);
    let data = query(&router,token,&format!("fragment issue on Issue {{{list_fields}}}{SEARCH}"),json!({"repo":"proj","owner":"gh-issues-alice","type":"ISSUE","limit":100,"query":"repo:gh-issues-alice/proj is:issue label:bug sort:created-desc"})).await;
    assert_eq!(data["search"]["issueCount"], 1);
    assert_eq!(data["search"]["nodes"][0]["title"], "Renamed");
    // gh 2.102.0's default `issue view` and its `--json` documents with every issue field.
    for text in [
        include_str!("cli/issue-view-default.graphql"),
        include_str!("cli/issue-view-all.graphql"),
    ] {
        let data = query(&router, token, text, vars.clone()).await;
        let issue = &data["repository"]["issue"];
        assert_eq!(issue["__typename"], "Issue");
        assert_eq!(issue["labels"]["nodes"][0]["name"], "bug");
        assert_eq!(issue["assignees"]["nodes"][0]["login"], "gh-issues-alice");
        assert_eq!(issue["subIssues"]["nodes"][0]["id"], child);
        assert_eq!(issue["blockedBy"]["totalCount"], 1);
    }
    let data = query(
        &router,
        token,
        include_str!("cli/issue-list-all.graphql"),
        json!({"owner":"gh-issues-alice","repo":"proj","limit":30,"states":["OPEN"]}),
    )
    .await;
    assert_eq!(data["repository"]["issues"]["totalCount"], 2);
    let listed = data["repository"]["issues"]["nodes"].as_array().unwrap();
    let all = listed.iter().find(|node| node["id"] == parent).unwrap();
    assert_eq!(all["subIssuesSummary"]["total"], 1);
    assert_eq!(all["projectItems"]["totalCount"], 0);
    let data = query(&router,token,include_str!("cli/issue-search-all.graphql"),json!({"limit":30,"owner":"gh-issues-alice","repo":"proj","type":"ISSUE_ADVANCED","query":"( label:bug ) repo:gh-issues-alice/proj state:open type:issue"})).await;
    assert_eq!(data["search"]["issueCount"], 1);
    assert_eq!(data["search"]["nodes"][0]["id"], parent);
    let data = query(
        &router,
        token,
        include_str!("cli/issue-status-all.graphql"),
        json!({"owner":"gh-issues-alice","repo":"proj","viewer":"gh-issues-alice"}),
    )
    .await;
    assert_eq!(data["repository"]["authored"]["totalCount"], 2);
    assert_eq!(data["repository"]["assigned"]["nodes"][0]["id"], parent);
    for (name, ty, field, input) in [
        (
            "IssueClose",
            "CloseIssueInput",
            "closeIssue",
            json!({"issueId":parent,"stateReason":"NOT_PLANNED"}),
        ),
        (
            "IssueReopen",
            "ReopenIssueInput",
            "reopenIssue",
            json!({"issueId":parent}),
        ),
    ] {
        let text =
            format!("mutation {name}($input:{ty}!){{{field}(input: $input){{issue{{id}}}}}}");
        assert_eq!(
            query(&router, token, &text, json!({"input":input})).await[field]["issue"]["id"],
            parent
        );
        let record = db::issues::get_issue_by_id(f.pool(), parent_record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            record.state,
            if name == "IssueClose" {
                "closed"
            } else {
                "open"
            }
        );
        if name == "IssueClose" {
            assert_eq!(record.state_reason.as_deref(), Some("not_planned"));
        }
    }
    for (name, ty, field, record, locked) in [
        (
            "LockLockable",
            "LockLockableInput",
            "lockLockable",
            "lockedRecord",
            true,
        ),
        (
            "UnlockLockable",
            "UnlockLockableInput",
            "unlockLockable",
            "unlockedRecord",
            false,
        ),
    ] {
        let text = format!(
            "mutation {name}($input:{ty}!){{{field}(input: $input){{{record}{{locked}}}}}}"
        );
        assert_eq!(
            query(
                &router,
                token,
                &text,
                json!({"input":{"lockableId":parent}})
            )
            .await[field][record]["locked"],
            locked
        );
    }
    for (name, ty, field, input) in [
        (
            "RemoveSubIssue",
            "RemoveSubIssueInput",
            "removeSubIssue",
            json!({"issueId":parent,"subIssueId":child}),
        ),
        (
            "RemoveBlockedBy",
            "RemoveBlockedByInput",
            "removeBlockedBy",
            json!({"issueId":parent,"blockingIssueId":child}),
        ),
    ] {
        let text =
            format!("mutation {name}($input:{ty}!){{{field}(input: $input){{issue{{id}}}}}}");
        assert_eq!(
            query(&router, token, &text, json!({"input":input})).await[field]["issue"]["id"],
            parent
        );
    }
    let text = "mutation LabelRemove($input:RemoveLabelsFromLabelableInput!){removeLabelsFromLabelable(input: $input){__typename}}";
    query(
        &router,
        token,
        text,
        json!({"input":{"labelableId":parent,"labelIds":[label_id]}}),
    )
    .await;
    assert!(
        db::labels::get_issue_labels(f.pool(), parent_record.repo_id, parent_record.id)
            .await
            .unwrap()
            .is_empty()
    );
    // gh issue edit --title --add-label sends both mutations at once, and a label change moves
    // the issue's update time while the title edit is saving.
    let edit =
        "mutation IssueUpdate($input:UpdateIssueInput!){updateIssue(input: $input){__typename}}";
    for (label_text, title) in [(add_label, "Renamed with a label"), (text, "Renamed by gh")] {
        tokio::join!(
            query(
                &router,
                token,
                edit,
                json!({"input":{"id":parent,"title":title}})
            ),
            query(
                &router,
                token,
                label_text,
                json!({"input":{"labelableId":parent,"labelIds":[label_id]}})
            ),
        );
    }
    assert_eq!(
        db::issues::get_issue_by_id(f.pool(), parent_record.id)
            .await
            .unwrap()
            .unwrap()
            .title,
        "Renamed by gh"
    );
    assert!(
        db::labels::get_issue_labels(f.pool(), parent_record.repo_id, parent_record.id)
            .await
            .unwrap()
            .is_empty()
    );
    let text = "mutation CommentDelete($input:DeleteIssueCommentInput!){deleteIssueComment(input: $input){clientMutationId}}";
    assert_eq!(
        query(&router, token, text, json!({"input":{"id":comment_id}})).await,
        json!({"deleteIssueComment":{"clientMutationId":null}})
    );
    assert!(
        db::issues::list_issue_comments(f.pool(), parent_record.id, 100, 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(db::relationships::list(
        f.pool(),
        Some(alice),
        parent_record.id,
        db::relationships::Kind::Children,
        0,
        100
    )
    .await
    .unwrap()
    .is_empty());
    assert!(db::relationships::list(
        f.pool(),
        Some(alice),
        parent_record.id,
        db::relationships::Kind::BlockedBy,
        0,
        100
    )
    .await
    .unwrap()
    .is_empty());
    assert_eq!(child_record.number, 2);
    for credential in [&credentials[1], &credentials[2]] {
        for (text,input) in [(add_label,json!({"labelableId":parent,"labelIds":[label_id]})),("mutation IssueUpdate($input:UpdateIssueInput!){updateIssue(input: $input){__typename}}",json!({"id":parent,"title":"Forbidden"}))] {
            let (_,body) = call(&router,Some(credential),text,json!({"input":input})).await;
            assert!(body["errors"].as_array().is_some_and(|e|!e.is_empty()),"{body}");
        }
    }
    let (_, body) = call(
        &router,
        Some(&credentials[1]),
        &lookup("id,title", "id,title"),
        vars,
    )
    .await;
    assert_eq!(body["errors"][0]["type"], "NOT_FOUND");

    let assignee = query(&router, token, "query RepositoryAssignableUsers($endCursor:String,$name:String!,$owner:String!){repository(owner: $owner, name: $name){assignableUsers(first: 100, after: $endCursor){nodes{id,login,name},pageInfo{hasNextPage,endCursor}}}}", json!({"name":"proj","owner":"gh-issues-alice"})).await["repository"]["assignableUsers"]["nodes"][0]["id"].clone();
    let milestone_id = query(&router,token,"query RepositoryMilestoneByNumber($name:String!,$number:Int!,$owner:String!){repository(owner: $owner, name: $name){milestone(number: $number){id,title}}}",json!({"name":"proj","owner":"gh-issues-alice","number":milestone.number})).await["repository"]["milestone"]["id"].clone();
    let data = query(&router, token, CREATE, json!({"input":{"repositoryId":repo_id,"title":"Metadata","assigneeIds":[assignee],"labelIds":[label_id],"milestoneId":milestone_id}})).await;
    let metadata_id = data["createIssue"]["issue"]["id"].clone();
    let metadata_record = db::issues::get_issue(f.pool(), repo.id, 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(metadata_record.milestone_id, Some(milestone.id));
    assert_eq!(
        db::assignees::list_assignees(
            f.pool(),
            db::assignees::AssignedItem::Issue(metadata_record.id)
        )
        .await
        .unwrap(),
        vec!["gh-issues-alice"]
    );
    assert_eq!(
        db::labels::get_issue_labels(f.pool(), metadata_record.repo_id, metadata_record.id)
            .await
            .unwrap()[0]
            .id,
        label.id
    );
    let update =
        "mutation IssueUpdate($input:UpdateIssueInput!){updateIssue(input: $input){__typename}}";
    query(
        &router,
        token,
        update,
        json!({"input":{"id":metadata_id,"assigneeIds":[],"labelIds":[],"milestoneId":null}}),
    )
    .await;
    assert!(db::issues::get_issue_by_id(f.pool(), metadata_record.id)
        .await
        .unwrap()
        .unwrap()
        .milestone_id
        .is_none());
    assert!(db::assignees::list_assignees(
        f.pool(),
        db::assignees::AssignedItem::Issue(metadata_record.id)
    )
    .await
    .unwrap()
    .is_empty());
    assert!(
        db::labels::get_issue_labels(f.pool(), metadata_record.repo_id, metadata_record.id)
            .await
            .unwrap()
            .is_empty()
    );
    query(&router, token, update, json!({"input":{"id":metadata_id,"assigneeIds":[assignee],"labelIds":[label_id],"milestoneId":milestone_id}})).await;
    let extra = query(
        &router,
        token,
        CREATE,
        json!({"input":{"repositoryId":repo_id,"title":"Fourth"}}),
    )
    .await["createIssue"]["issue"]["id"]
        .clone();
    let pin = "mutation IssuePin($input:PinIssueInput!){pinIssue(input: $input){issue{id}}}";
    let unpin =
        "mutation IssueUnpin($input:UnpinIssueInput!){unpinIssue(input: $input){issue{id}}}";
    for id in [&parent, &child, &metadata_id] {
        assert_eq!(
            query(&router, token, pin, json!({"input":{"issueId":id}})).await["pinIssue"]["issue"]
                ["id"],
            *id
        );
    }
    query(&router, token, pin, json!({"input":{"issueId":parent}})).await;
    let (_, body) = call(
        &router,
        Some(token),
        pin,
        json!({"input":{"issueId":extra}}),
    )
    .await;
    assert!(body["errors"].is_array(), "{body}");
    let data = query(
        &router,
        token,
        &lookup("id,isPinned", "id"),
        json!({"owner":"gh-issues-alice","repo":"proj","number":3}),
    )
    .await;
    assert_eq!(data["repository"]["issue"]["isPinned"], true);
    assert_eq!(
        query(&router, token, unpin, json!({"input":{"issueId":parent}})).await["unpinIssue"]
            ["issue"]["id"],
        parent
    );
    let native = Client::login(&router, f.pool(), alice).await;
    let list = native
        .exchange("GET", "/gh-issues-alice/proj/issues", "", vec![])
        .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.text());
    assert!(list.text().contains("Pinned issues"));
    let detail = native
        .exchange("GET", "/gh-issues-alice/proj/issues/3", "", vec![])
        .await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.text());
    for control in [
        "Unpin issue",
        "Transfer issue",
        "Delete issue",
        "Create a branch",
        "Close as duplicate",
    ] {
        assert!(detail.text().contains(control), "Missing {control}");
    }
    let forbidden = native
        .exchange(
            "POST",
            "/gh-issues-alice/proj/issues/3/manage",
            "application/x-www-form-urlencoded",
            b"operation=unpin".to_vec(),
        )
        .await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
    let reply = native
        .form_reply(
            "/gh-issues-alice/proj/issues/3/manage",
            &[("operation", "unpin")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let close =
        "mutation IssueClose($input:CloseIssueInput!){closeIssue(input: $input){issue{id}}}";
    assert_eq!(query(&router,token,close,json!({"input":{"issueId":metadata_id,"duplicateIssueId":parent,"stateReason":"DUPLICATE"}})).await["closeIssue"]["issue"]["id"],metadata_id);
    assert_eq!(
        db::issues::management::duplicate(f.pool(), metadata_record.id)
            .await
            .unwrap()
            .unwrap()
            .id,
        parent_record.id
    );
    assert_eq!(
        db::issues::get_issue_by_id(f.pool(), metadata_record.id)
            .await
            .unwrap()
            .unwrap()
            .state_reason
            .as_deref(),
        Some("duplicate")
    );
    let (_, body) = call(
        &router,
        Some(token),
        close,
        json!({"input":{"issueId":parent,"duplicateIssueId":parent}}),
    )
    .await;
    assert!(body["errors"].is_array());
    let branch = "mutation CreateLinkedBranch($input:CreateLinkedBranchInput!){createLinkedBranch(input: $input){linkedBranch{id,ref{name}}}}";
    let data = query(&router,token,branch,json!({"input":{"issueId":metadata_id,"name":"3-metadata","oid":saved["commit"]["sha"],"repositoryId":repo_id}})).await;
    keys(&data["createLinkedBranch"]["linkedBranch"], &["id", "ref"]);
    assert_eq!(
        data["createLinkedBranch"]["linkedBranch"]["ref"]["name"],
        "3-metadata"
    );
    let linked = "query ListLinkedBranches($name:String!$number:Int!$owner:String!){repository(owner: $owner, name: $name){issue(number: $number){linkedBranches(first: 30){nodes{ref{name,repository{url}}}}}}}";
    let data = query(
        &router,
        token,
        linked,
        json!({"name":"proj","owner":"gh-issues-alice","number":3}),
    )
    .await;
    let nodes = data["repository"]["issue"]["linkedBranches"]["nodes"]
        .as_array()
        .unwrap();
    assert_eq!(nodes.len(), 1);
    keys(&nodes[0]["ref"], &["name", "repository"]);
    assert_eq!(nodes[0]["ref"]["name"], "3-metadata");
    assert!(nodes[0]["ref"]["repository"]["url"]
        .as_str()
        .unwrap()
        .ends_with("/gh-issues-alice/proj"));
    let pull = db::pull_requests::records::create_pull_request(
        f.pool(),
        db::pull_requests::records::NewPullRequest {
            issue: None,
            maintainer_can_modify: false,
            repo_id: repo.id,
            head_repo_id: Some(repo.id),
            author_id: alice,
            title: "Fix the issue",
            body: "Fixes #3",
            head_branch: "3-metadata",
            head_sha: saved["commit"]["sha"].as_str().unwrap(),
            base_branch: &repo.default_branch,
            base_sha: saved["commit"]["sha"].as_str().unwrap(),
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let closing = "query IssueByNumber($owner:String!,$repo:String!,$number:Int!){repository(owner: $owner, name: $repo){hasIssuesEnabled,issue:issueOrPullRequest(number: $number){__typename,...on Issue{closedByPullRequestsReferences(first:100){nodes{id,number,url,repository{id,name,owner{id,login}}},pageInfo{hasNextPage,endCursor}}},...on PullRequest{id}}}}";
    let data = query(
        &router,
        token,
        closing,
        json!({"owner":"gh-issues-alice","repo":"proj","number":3}),
    )
    .await;
    let refs = &data["repository"]["issue"]["closedByPullRequestsReferences"];
    keys(&refs["nodes"][0], &["id", "number", "url", "repository"]);
    keys(&refs["nodes"][0]["repository"], &["id", "name", "owner"]);
    keys(&refs["nodes"][0]["repository"]["owner"], &["id", "login"]);
    keys(&refs["pageInfo"], &["hasNextPage", "endCursor"]);
    assert_eq!(refs["nodes"][0]["number"], pull.number);
    let issue_url = query(
        &router,
        token,
        &lookup("url", "url"),
        json!({"owner":"gh-issues-alice","repo":"proj","number":3}),
    )
    .await["repository"]["issue"]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    for (body, count) in [
        ("Closes #3".to_owned(), 1),
        ("Fixed gh-issues-alice/proj#3".to_owned(), 1),
        (format!("Resolves {issue_url}"), 1),
        ("Mentions #3".to_owned(), 0),
        ("Fixes #30".to_owned(), 0),
        ("Fixes another/project#3".to_owned(), 0),
        (
            "Fixes https://external.test/gh-issues-alice/proj/issues/3".to_owned(),
            0,
        ),
    ] {
        f.execute(
            &format!(
                "UPDATE pull_requests SET body='{}' WHERE id=$1",
                body.replace('\'', "''")
            ),
            &[pull.id],
        )
        .await;
        let data = query(
            &router,
            token,
            closing,
            json!({"owner":"gh-issues-alice","repo":"proj","number":3}),
        )
        .await;
        assert_eq!(
            data["repository"]["issue"]["closedByPullRequestsReferences"]["nodes"]
                .as_array()
                .unwrap()
                .len(),
            count,
            "{body}"
        );
    }
    let target = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "destination",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let target_id = query(
        &router,
        token,
        REPOSITORY,
        json!({"owner":"gh-issues-alice","name":"destination"}),
    )
    .await["repository"]["id"]
        .clone();
    let target_label = db::labels::create_label(f.pool(), alice, target.id, "bug", "111111", None)
        .await
        .unwrap();
    let target_milestone =
        db::milestones::create_milestone(f.pool(), alice, target.id, "Release", None, None)
            .await
            .unwrap();
    let transfer = "mutation IssueTransfer($input:TransferIssueInput!){transferIssue(input: $input){issue{url}}}";
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let delete = "mutation IssueDelete($input:DeleteIssueInput!){deleteIssue(input: $input){repository{id}}}";
    for (text, input) in [
        (delete, json!({"issueId":metadata_id})),
        (
            transfer,
            json!({"issueId":metadata_id,"repositoryId":target_id}),
        ),
    ] {
        let (_, body) = call(&router, Some(&credentials[1]), text, json!({"input":input})).await;
        assert!(body["errors"].is_array(), "{body}");
    }
    f.execute("INSERT INTO projects(owner_user_id,number,creator_id,title,created_at,updated_at) VALUES($1,1,$1,'Board',CURRENT_TIMESTAMP,CURRENT_TIMESTAMP)", &[alice]).await;
    f.execute("INSERT INTO project_items(project_id,creator_id,repo_id,issue_id,created_at,updated_at) SELECT id,$1,$2,$3,CURRENT_TIMESTAMP,CURRENT_TIMESTAMP FROM projects WHERE owner_user_id=$1", &[alice, repo.id, metadata_record.id]).await;
    let data = query(
        &router,
        token,
        transfer,
        json!({"input":{"issueId":metadata_id,"repositoryId":target_id}}),
    )
    .await;
    assert!(data["transferIssue"]["issue"]["url"]
        .as_str()
        .unwrap()
        .ends_with("/destination/issues/1"));
    let moved = db::issues::get_issue_by_id(f.pool(), metadata_record.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved.repo_id, target.id);
    assert_eq!(moved.milestone_id, Some(target_milestone.id));
    // The issue keeps its place in the owner's project.
    assert_eq!(
        f.scalar(
            "SELECT repo_id FROM project_items WHERE issue_id=$1",
            &[moved.id]
        )
        .await,
        target.id
    );
    assert_eq!(
        db::labels::get_issue_labels(f.pool(), moved.repo_id, moved.id)
            .await
            .unwrap()[0]
            .id,
        target_label.id
    );
    let redirect = native
        .exchange("GET", "/gh-issues-alice/proj/issues/3", "", vec![])
        .await;
    assert_eq!(
        redirect.status,
        StatusCode::SEE_OTHER,
        "{}",
        redirect.text()
    );
    assert_eq!(
        redirect.location.as_deref(),
        Some("/gh-issues-alice/destination/issues/1")
    );
    query(&router,token,close,json!({"input":{"issueId":parent,"duplicateIssueId":metadata_id,"stateReason":"DUPLICATE"}})).await;
    assert_eq!(
        db::issues::management::duplicate(f.pool(), parent_record.id)
            .await
            .unwrap()
            .unwrap()
            .id,
        metadata_record.id
    );
    let (_, denied) = call(
        &router,
        Some(&credentials[1]),
        close,
        json!({"input":{"issueId":child,"duplicateIssueId":metadata_id,"stateReason":"DUPLICATE"}}),
    )
    .await;
    assert!(denied["errors"].is_array(), "{denied}");
    assert_eq!(
        db::issues::get_issue_by_id(f.pool(), child_record.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "open"
    );
    assert_eq!(
        query(
            &router,
            token,
            delete,
            json!({"input":{"issueId":metadata_id}})
        )
        .await["deleteIssue"]["repository"]["id"],
        target_id
    );
    assert!(db::issues::get_issue_by_id(f.pool(), metadata_record.id)
        .await
        .unwrap()
        .is_none());
    assert!(
        db::issues::management::linked_branches(f.pool(), metadata_record.id)
            .await
            .unwrap()
            .is_empty()
    );
}

// The contract's debug-build future outgrows the default test stack.
#[test]
fn sqlite_gh_issues_queries() {
    crate::support::large_stack::run(|| contract(false));
}
#[test]
fn postgres_gh_issues_queries() {
    crate::support::large_stack::run(|| contract(true));
}

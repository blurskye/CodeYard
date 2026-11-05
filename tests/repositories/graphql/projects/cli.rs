//! Wire documents emitted by gh 2.102.0 from pkg/cmd/project and api/queries_repo.go.
use super::*;
use async_graphql_parser::{
    parse_query,
    types::{ExecutableDocument, Selection, SelectionSet},
};
use codeyard_identity::tokens;
use codeyard_model::node_id;

macro_rules! document {
    ($name:literal) => {
        include_str!(concat!("queries/", $name, ".graphql"))
    };
}

fn selected(document: &ExecutableDocument, selection: &SelectionSet, data: &Value) {
    if data.is_null() {
        return;
    }
    if let Some(values) = data.as_array() {
        for value in values {
            selected(document, selection, value);
        }
        return;
    }
    for entry in &selection.items {
        match &entry.node {
            Selection::Field(field) => {
                let key = field.node.response_key().node.as_str();
                assert!(data.get(key).is_some(), "Missing {key}: {data}");
                if !field.node.selection_set.node.items.is_empty() {
                    selected(document, &field.node.selection_set.node, &data[key]);
                }
            }
            Selection::InlineFragment(fragment) => {
                if fragment
                    .node
                    .type_condition
                    .as_ref()
                    .is_none_or(|condition| {
                        data["__typename"]
                            .as_str()
                            .is_none_or(|ty| ty == condition.node.on.node.as_str())
                    })
                {
                    selected(document, &fragment.node.selection_set.node, data);
                }
            }
            Selection::FragmentSpread(spread) => selected(
                document,
                &document.fragments[&spread.node.fragment_name.node]
                    .node
                    .selection_set
                    .node,
                data,
            ),
        }
    }
}

pub(super) async fn send(router: &Router, token: &str, text: &str, variables: Value) -> Value {
    let (status, body) = call(router, Some(token), text, variables).await;
    assert_eq!(status, StatusCode::OK, "{text}: {body}");
    assert!(body.get("errors").is_none(), "{text}: {body}");
    let document = parse_query(text).unwrap();
    selected(
        &document,
        &document
            .operations
            .iter()
            .next()
            .unwrap()
            .1
            .node
            .selection_set
            .node,
        &body["data"],
    );
    body["data"].clone()
}

async fn cli_contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("project-cli-alice").await;
    let bob = f.actor("project-cli-bob").await;
    let org = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            f.pool(),
            alice,
            &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
                "project-cli-org",
                codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let team = applied(
        codeyard_db_orgs::organizations::teams::create_organization_team(
            f.pool(),
            alice,
            org.id,
            &codeyard_db_orgs::organizations::teams::NewTeam::new(
                "planning",
                codeyard_db_orgs::organizations::teams::TeamProfile::new(
                    "Planning",
                    None,
                    codeyard_db_orgs::organizations::teams::TeamPrivacy::Visible,
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_repos_core::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
        "plan",
        Some("Project repository"),
        true,
        None,
    )
    .await
    .unwrap();
    let issue =
        codeyard_db_repos::issues::create_issue(f.pool(), repo.id, alice, "Issue", "Issue body")
            .await
            .unwrap();
    let pull = codeyard_db_repos::pull_requests::records::create_pull_request(
        f.pool(),
        codeyard_db_repos::pull_requests::records::NewPullRequest {
            repo_id: repo.id,
            head_repo_id: Some(repo.id),
            author_id: alice,
            title: "Pull",
            body: "Pull body",
            head_branch: "feature",
            head_sha: "1111111111111111111111111111111111111111",
            base_branch: "main",
            base_sha: "2222222222222222222222222222222222222222",
            is_draft: false,
            issue: None,
            maintainer_can_modify: false,
        },
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let mut credentials = Vec::new();
    for (actor, scope) in [(alice, "write"), (alice, "read"), (bob, "write")] {
        let session = Client::login(&router, f.pool(), actor).await;
        credentials.push(
            tokens::create(
                f.pool(),
                actor,
                Some(&session.session),
                TokenInput {
                    name: "project-cli",
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
    for text in [document!("Viewer"), document!("ViewerOwner")] {
        let data = send(&router, token, text, json!({})).await;
        assert_eq!(data["viewer"]["login"], "project-cli-alice");
        assert_eq!(data["viewer"]["id"], node_id::encode("U", alice));
    }
    let logins = send(
        &router,
        token,
        document!("ViewerLoginAndOrgs"),
        json!({"after":null}),
    )
    .await;
    assert_eq!(
        logins["viewer"]["organizations"]["nodes"][0]["login"],
        org.name
    );
    assert_eq!(
        logins["viewer"]["organizations"]["nodes"][0]["viewerCanCreateProjects"],
        true
    );
    for (login, present, absent) in [
        ("project-cli-alice", "user", "organization"),
        ("project-cli-org", "organization", "user"),
    ] {
        let (status, body) = call(
            &router,
            Some(token),
            document!("UserOrgOwner"),
            json!({"login":login}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"][present]["login"], login);
        assert!(body["data"][present]["id"].is_string());
        assert!(body["data"][absent].is_null());
        assert_eq!(body["errors"].as_array().unwrap().len(), 1);
        assert_eq!(body["errors"][0]["type"], "NOT_FOUND");
        assert_eq!(body["errors"][0]["path"][0], absent);
    }
    let counts = json!({"firstItems":0,"afterItems":null,"firstFields":0,"afterFields":null});
    let mut variables = counts.clone();
    variables["input"] = json!({"ownerId":node_id::encode("U",alice),"title":"CLI plan"});
    let created = send(
        &router,
        token,
        document!("CreateProjectV2"),
        variables.clone(),
    )
    .await;
    let pid = created["createProjectV2"]["projectV2"]["id"].clone();
    assert_eq!(
        created["createProjectV2"]["projectV2"]["fields"]["totalCount"],
        8
    );
    assert_eq!(
        created["createProjectV2"]["projectV2"]["items"]["totalCount"],
        0
    );
    let project_id = codeyard_db_repos::projects::get(
        f.pool(),
        Some(alice),
        codeyard_db_repos::projects::Owner::User(alice),
        1,
    )
    .await
    .unwrap()
    .id;
    assert_eq!(pid, node_id::scoped("PVT", alice, project_id).unwrap());
    let computed = node_id::project_field(alice, project_id, "ASSIGNEES").unwrap();
    for id in [
        computed.clone(),
        node_id::git("PVTBF", &format!("{project_id}/ASSIGNEES")),
    ] {
        let found = send(
            &router,
            token,
            "query($id:ID!){node(id:$id){id ... on ProjectV2Field{name dataType project{id}}}}",
            json!({"id":id}),
        )
        .await;
        assert_eq!(found["node"]["id"], computed);
        assert_eq!(found["node"]["dataType"], "ASSIGNEES");
        assert_eq!(found["node"]["project"]["id"], pid);
    }
    let (_, rejected) = call(
        &router,
        Some(token),
        "query($id:ID!){node(id:$id){id}}",
        json!({"id":node_id::project_field(alice + 10000, project_id, "ASSIGNEES").unwrap()}),
    )
    .await;
    assert!(rejected["data"]["node"].is_null(), "{rejected}");
    assert_eq!(rejected["errors"][0]["type"], "NOT_FOUND", "{rejected}");
    let org_project = codeyard_db_repos::projects::create(
        f.pool(),
        alice,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(f.pool(), alice).await,
        ),
        codeyard_db_repos::projects::Owner::Organization(org.id),
        "Organization plan",
        None,
        false,
    )
    .await
    .unwrap();
    let draft = send(
        &router,
        token,
        document!("CreateDraftItem"),
        json!({"input":{"projectId":pid,"title":"Draft","body":"Draft body"}}),
    )
    .await;
    let item = draft["addProjectV2DraftIssue"]["projectItem"]["id"].clone();
    let did = draft["addProjectV2DraftIssue"]["projectItem"]["content"]["id"].clone();
    let draft_row = node_id::routed(did.as_str().unwrap()).unwrap().id;
    let stored_fields = codeyard_db_repos::projects::fields(f.pool(), Some(alice), project_id)
        .await
        .unwrap();
    for (kind, row) in [
        ("PVT", project_id),
        ("PVTF", stored_fields[0].id),
        ("PVTI", draft_row),
        ("DI", draft_row),
    ] {
        let rooted = node_id::scoped(kind, alice, row).unwrap();
        let found = send(
            &router,
            token,
            "query($id:ID!){node(id:$id){id}}",
            json!({"id":rooted}),
        )
        .await;
        assert_eq!(found["node"]["id"], rooted);
        let (_, rejected) = call(
            &router,
            Some(token),
            "query($id:ID!){node(id:$id){id}}",
            json!({"id":node_id::scoped(kind, alice + 10000, row).unwrap()}),
        )
        .await;
        assert!(rejected["data"]["node"].is_null(), "{rejected}");
        assert_eq!(rejected["errors"][0]["type"], "NOT_FOUND", "{rejected}");
    }
    let (_, rejected) = call(&router, Some(token),
        "mutation($input:UpdateProjectV2Input!){updateProjectV2(input:$input){projectV2{title}}}",
        json!({"input":{"projectId":node_id::scoped("PVT",alice + 10000,project_id).unwrap(),"title":"forged root"}})).await;
    assert!(rejected.get("errors").is_some(), "{rejected}");
    assert_eq!(
        codeyard_db_repos::projects::by_id(f.pool(), Some(alice), project_id)
            .await
            .unwrap()
            .title,
        "CLI plan"
    );

    assert_eq!(
        draft["addProjectV2DraftIssue"]["projectItem"]["content"]["body"],
        "Draft body"
    );
    let current = send(
        &router,
        token,
        document!("DraftIssueByID"),
        json!({"id":did}),
    )
    .await;
    assert_eq!(current["node"]["title"], "Draft");
    let edited = send(
        &router,
        token,
        document!("EditDraftIssueItem"),
        json!({"input":{"draftIssueId":did,"title":"Draft edited","body":"Edited body"}}),
    )
    .await;
    assert_eq!(
        edited["updateProjectV2DraftIssue"]["draftIssue"]["body"],
        "Edited body"
    );
    for (path, number, expected_id, expected_type) in [
        (
            "issues",
            issue.number,
            node_id::scoped("I", repo.id, issue.id).unwrap(),
            "Issue",
        ),
        (
            "pull",
            pull.number,
            node_id::scoped("PR", repo.id, pull.id).unwrap(),
            "PullRequest",
        ),
    ] {
        let content_url = format!("http://localhost:3000/project-cli-alice/plan/{path}/{number}");
        let found = send(
            &router,
            token,
            document!("GetIssueOrPullRequest"),
            json!({"url":content_url}),
        )
        .await;
        assert_eq!(found["resource"]["id"], expected_id);
        assert_eq!(found["resource"]["__typename"], expected_type);
        let added = send(
            &router,
            token,
            document!("AddItem"),
            json!({"input":{"projectId":pid,"contentId":expected_id}}),
        )
        .await;
        assert_eq!(
            added["addProjectV2ItemById"]["item"]["content"]["repository"]["nameWithOwner"],
            "project-cli-alice/plan"
        );
        let found = send(
            &router,
            token,
            document!("GetProjectItemByURL"),
            json!({"url":content_url,"firstItems":100}),
        )
        .await;
        assert_eq!(
            found["resource"]["projectItems"]["nodes"][0]["project"]["id"],
            pid
        );
        assert_eq!(
            found["resource"]["projectItems"]["nodes"][0]["id"],
            added["addProjectV2ItemById"]["item"]["id"]
        );
    }
    // Built-in fields carry the linked content's labels and repository.
    let label = codeyard_db_repos::labels::create_label(
        f.pool(),
        alice,
        repo.id,
        "planning",
        "#123456",
        None,
    )
    .await
    .unwrap();
    codeyard_db_repos::labels::add_issue_label(f.pool(), alice, repo.id, issue.id, label.id)
        .await
        .unwrap();
    let listed = send(
        &router,
        token,
        document!("UserProjectWithItems"),
        json!({"login":"project-cli-alice","number":1,"firstItems":100,"afterItems":null,"firstFields":100,"afterFields":null}),
    )
    .await;
    let items = listed["user"]["projectV2"]["items"]["nodes"]
        .as_array()
        .unwrap();
    for (ty, labelled) in [("Issue", true), ("PullRequest", false)] {
        let values = items
            .iter()
            .find(|item| item["content"]["__typename"] == ty)
            .unwrap()["fieldValues"]["nodes"]
            .as_array()
            .unwrap();
        assert!(values.iter().any(|v| v["field"]["dataType"] == "REPOSITORY"
            && v["repository"]["url"]
                .as_str()
                .unwrap()
                .ends_with("/project-cli-alice/plan")));
        assert_eq!(
            values.iter().any(|v| v["field"]["dataType"] == "LABELS"
                && v["labels"]["nodes"][0]["name"] == "planning"),
            labelled,
            "{values:?}"
        );
    }
    let mut fields = Vec::new();
    for (name, data_type) in [
        ("Notes", "TEXT"),
        ("Estimate", "NUMBER"),
        ("Due", "DATE"),
        ("Priority", "SINGLE_SELECT"),
    ] {
        let mut input = json!({"projectId":pid,"name":name,"dataType":data_type});
        if data_type == "SINGLE_SELECT" {
            input["singleSelectOptions"] = json!([{"name":"High","color":"GRAY","description":""}]);
        }
        let field = send(
            &router,
            token,
            document!("CreateField"),
            json!({"input":input}),
        )
        .await["createProjectV2Field"]["projectV2Field"]
            .clone();
        assert_eq!(field["name"], name);
        assert_eq!(field["dataType"], data_type);
        fields.push(field);
    }
    for (field, value, key, expected) in [
        (&fields[0], json!({"text":"Notes"}), "text", json!("Notes")),
        (&fields[1], json!({"number":8.5}), "number", json!(8.5)),
        (
            &fields[2],
            json!({"date":"2026-10-24T00:00:00Z"}),
            "date",
            json!("2026-10-24"),
        ),
        (
            &fields[3],
            json!({"singleSelectOptionId":fields[3]["options"][0]["id"]}),
            "name",
            json!("High"),
        ),
    ] {
        let data = send(
            &router,
            token,
            document!("UpdateItemValues"),
            json!({"input":{"projectId":pid,"itemId":item,"fieldId":field["id"],"value":value}}),
        )
        .await;
        let values = data["updateProjectV2ItemFieldValue"]["projectV2Item"]["fieldValues"]["nodes"]
            .as_array()
            .unwrap();
        assert!(
            values
                .iter()
                .any(|v| v["field"]["id"] == field["id"] && v[key] == expected),
            "{values:?}"
        );
    }
    // Iteration fields already exist in the native store, even though gh cannot create them.
    let iteration = codeyard_db_repos::projects::add_field(
        f.pool(),
        alice,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(f.pool(), alice).await,
        ),
        project_id,
        &codeyard_db_repos::projects::NewField {
            name: "Sprint".into(),
            data_type: "iteration".into(),
            single_select_options: None,
            iteration_configuration: Some(codeyard_db_repos::projects::IterationConfiguration {
                start_date: "2026-10-24".parse().unwrap(),
                duration: 14,
                iterations: vec![codeyard_db_repos::projects::Iteration {
                    title: "Sprint 1".into(),
                    start_date: "2026-10-24".parse().unwrap(),
                    duration: 14,
                }],
            }),
        },
    )
    .await
    .unwrap();
    let config = codeyard_db_repos::projects::field_configuration(&iteration).unwrap();
    let data = send(&router,token,document!("UpdateItemValues"),json!({"input":{"projectId":pid,"itemId":item,"fieldId":node_id::encode("PVTIF",iteration.id),"value":{"iterationId":config["iterations"][0]["id"]}}})).await;
    assert!(
        data["updateProjectV2ItemFieldValue"]["projectV2Item"]["fieldValues"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["title"] == "Sprint 1" && v["duration"] == 14)
    );
    codeyard_db_repos::projects::add_item(
        f.pool(),
        alice,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(f.pool(), alice).await,
        ),
        org_project.id,
        &codeyard_db_repos::projects::Content::Draft {
            title: "Org draft".into(),
            body: Some("Body".into()),
        },
    )
    .await
    .unwrap();
    let owner_queries = [
        (
            "viewer",
            "project-cli-alice",
            document!("ViewerProject"),
            document!("ViewerProjects"),
            document!("ViewerProjectWithFields"),
            document!("ViewerProjectWithItems"),
            document!("ViewerProjectWithItems-query"),
        ),
        (
            "user",
            "project-cli-alice",
            document!("UserProject"),
            document!("UserProjects"),
            document!("UserProjectWithFields"),
            document!("UserProjectWithItems"),
            document!("UserProjectWithItems-query"),
        ),
        (
            "organization",
            "project-cli-org",
            document!("OrgProject"),
            document!("OrgProjects"),
            document!("OrgProjectWithFields"),
            document!("OrgProjectWithItems"),
            document!("OrgProjectWithItems-query"),
        ),
    ];
    for (owner, login, one, list, with_fields, with_items, filtered) in owner_queries {
        let vars = json!({"login":login,"number":1,"first":1,"after":null,"firstItems":100,"afterItems":null,"firstFields":100,"afterFields":null,"query":"Draft"});
        for text in [one, with_fields, with_items, filtered] {
            let data = send(&router, token, text, vars.clone()).await;
            assert!(
                data[owner]["projectV2"]["fields"]["totalCount"]
                    .as_i64()
                    .unwrap()
                    >= 2
            );
            assert!(!data[owner]["projectV2"]["items"]["nodes"]
                .as_array()
                .unwrap()
                .is_empty());
            let mut page_vars = vars.clone();
            page_vars["firstFields"] = json!(1);
            page_vars["firstItems"] = json!(1);
            let page = send(&router, token, text, page_vars.clone()).await;
            page_vars["afterFields"] =
                page[owner]["projectV2"]["fields"]["pageInfo"]["endCursor"].clone();
            page_vars["afterItems"] =
                page[owner]["projectV2"]["items"]["pageInfo"]["endCursor"].clone();
            send(&router, token, text, page_vars).await;
        }
        let data = send(&router, token, list, vars.clone()).await;
        assert_eq!(
            data[owner]["projectsV2"]["nodes"].as_array().unwrap().len(),
            1
        );
        let mut next = vars;
        next["after"] = data[owner]["projectsV2"]["pageInfo"]["endCursor"].clone();
        let data = send(&router, token, list, next).await;
        assert!(data[owner]["projectsV2"]["nodes"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    let features = send(&router, token, document!("ProjectV2_fields"), json!({})).await;
    assert!(features["ProjectV2"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .any(|field| field["name"] == "items"
            && field["args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|arg| arg["name"] == "query")));
    let repo_info = send(
        &router,
        token,
        document!("RepositoryInfo"),
        json!({"owner":"project-cli-alice","name":"plan"}),
    )
    .await;
    assert_eq!(repo_info["repository"]["id"], node_id::encode("R", repo.id));
    assert_eq!(repo_info["repository"]["viewerPermission"], "ADMIN");
    assert_eq!(repo_info["repository"]["hasIssuesEnabled"], true);
    assert_eq!(repo_info["repository"]["hasWikiEnabled"], false);
    assert_eq!(
        repo_info["repository"]["mergeCommitAllowed"],
        repo.allow_merge_commit
    );
    for text in [
        document!("LinkProjectV2ToRepository"),
        document!("UnlinkProjectV2FromRepository"),
    ] {
        send(&router,token,text,json!({"input":{"projectId":pid,"repositoryId":node_id::encode("R",repo.id),"clientMutationId":"repository"}})).await;
    }
    let team_info = send(
        &router,
        token,
        document!("OrganizationTeam"),
        json!({"owner":org.name,"teamSlug":"planning"}),
    )
    .await;
    assert_eq!(team_info["organization"]["team"]["slug"], "planning");
    let tid = team_info["organization"]["team"]["id"].clone();
    assert_eq!(tid, node_id::scoped("T", org.id, team.id).unwrap());
    for text in [
        document!("LinkProjectV2ToTeam"),
        document!("UnlinkProjectV2FromTeam"),
    ] {
        let data = send(&router,token,text,json!({"input":{"projectId":node_id::encode("PVT",org_project.id),"teamId":tid,"clientMutationId":"team"}})).await;
        assert_eq!(
            data.as_object().unwrap().values().next().unwrap()["clientMutationId"],
            "team"
        );
    }
    for text in [
        document!("ArchiveProjectItem"),
        document!("UnarchiveProjectItem"),
    ] {
        let data = send(
            &router,
            token,
            text,
            json!({"input":{"projectId":pid,"itemId":item}}),
        )
        .await;
        assert_eq!(
            data.as_object().unwrap().values().next().unwrap()["item"]["id"],
            item
        );
    }
    let cleared = send(
        &router,
        token,
        document!("ClearItemFieldValue"),
        json!({"input":{"projectId":pid,"itemId":item,"fieldId":fields[0]["id"]}}),
    )
    .await;
    assert!(
        !cleared["clearProjectV2ItemFieldValue"]["projectV2Item"]["fieldValues"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["field"]["id"] == fields[0]["id"])
    );
    for (text, input) in [
        (
            document!("UpdateProjectV2"),
            json!({"projectId":pid,"title":"Edited plan","shortDescription":"Description","readme":"Readme","public":true}),
        ),
        (
            document!("CloseProjectV2"),
            json!({"projectId":pid,"closed":true}),
        ),
        (document!("MarkProjectTemplate"), json!({"projectId":pid})),
        (document!("UnmarkProjectTemplate"), json!({"projectId":pid})),
    ] {
        let mut vars = counts.clone();
        vars["input"] = input;
        let data = send(&router, token, text, vars).await;
        assert_eq!(
            data.as_object().unwrap().values().next().unwrap()["projectV2"]["title"],
            "Edited plan"
        );
    }
    variables["input"] = json!({"projectId":pid,"ownerId":node_id::encode("U",alice),"title":"Copy","includeDraftIssues":true});
    let copied = send(
        &router,
        token,
        document!("CopyProjectV2"),
        variables.clone(),
    )
    .await;
    assert_eq!(
        copied["copyProjectV2"]["projectV2"]["items"]["totalCount"],
        1
    );
    assert_eq!(copied["copyProjectV2"]["projectV2"]["public"], false);
    for credential in [&credentials[1], &credentials[2]] {
        let (_, body) = call(
            &router,
            Some(credential),
            document!("UpdateProjectV2"),
            json!({"firstItems":0,"firstFields":0,"input":{"projectId":pid,"title":"Denied"}}),
        )
        .await;
        assert!(body.get("errors").is_some(), "{body}");
        let (_, body) = call(
            &router,
            Some(credential),
            document!("LinkProjectV2ToTeam"),
            json!({"input":{"projectId":node_id::encode("PVT",org_project.id),"teamId":tid}}),
        )
        .await;
        assert!(body.get("errors").is_some(), "{body}");
    }
    let (_, wrong_owner) = call(
        &router,
        Some(token),
        document!("LinkProjectV2ToTeam"),
        json!({"input":{"projectId":pid,"teamId":tid}}),
    )
    .await;
    assert!(wrong_owner.get("errors").is_some());
    send(
        &router,
        token,
        document!("DeleteField"),
        json!({"input":{"fieldId":fields[0]["id"]}}),
    )
    .await;
    let deleted = send(
        &router,
        token,
        document!("DeleteProjectItem"),
        json!({"input":{"projectId":pid,"itemId":item}}),
    )
    .await;
    assert_eq!(deleted["deleteProjectV2Item"]["deletedItemId"], item);
    variables["input"] = json!({"projectId":pid});
    let deleted = send(&router, token, document!("DeleteProject"), variables).await;
    assert_eq!(
        deleted["deleteProjectV2"]["projectV2"]["title"],
        "Edited plan"
    );
    assert_eq!(
        deleted["deleteProjectV2"]["projectV2"]["items"]["totalCount"],
        2
    );
    let (_, hidden) = call(
        &router,
        Some(token),
        document!("ViewerProject"),
        json!({"number":1,"firstItems":0,"firstFields":0}),
    )
    .await;
    assert!(hidden.get("errors").is_some());
}

#[tokio::test]
async fn graphql_projects_cli_sqlite() {
    cli_contract(false).await;
}
#[tokio::test]
async fn graphql_projects_cli_postgres() {
    cli_contract(true).await;
}

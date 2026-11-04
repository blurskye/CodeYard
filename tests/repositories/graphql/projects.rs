//! gh project's actual GraphQL shapes through the token-authenticated router.
mod cli;
mod metadata;
mod selections;
use super::*;
use selections::{FIELD_SELECTION, ITEM_SELECTION, PROJECT_SELECTION};

async fn project_mutation(
    router: &Router,
    token: &str,
    name: &str,
    input_type: &str,
    input: Value,
) -> Value {
    let doc = format!("mutation($input:{input_type}!) {{ {name}(input:$input) {{ clientMutationId projectV2 {{ ...ProjectCLI template }} }} }} {PROJECT_SELECTION}");
    query(router, token, &doc, json!({"input":input})).await[name]["projectV2"].clone()
}

async fn projects_contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("project-gql-alice").await;
    let bob = f.actor("project-gql-bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = create_router(super::patient(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    )));
    let mut credentials = Vec::new();
    for (actor, scope) in [(alice, "write"), (alice, "read"), (bob, "write")] {
        let session = Client::login(&router, f.pool(), actor).await;
        credentials.push(
            tokens::create(
                f.pool(),
                actor,
                Some(&session.session),
                TokenInput {
                    name: "gh-project",
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
    let owners = query(&router,token,"{ viewer { id login viewerCanCreateProjects organizations(first:100) { totalCount nodes { id login viewerCanCreateProjects } pageInfo { hasNextPage endCursor } } } }",json!({})).await;
    assert_eq!(owners["viewer"]["viewerCanCreateProjects"], true);
    let project = project_mutation(
        &router,
        token,
        "createProjectV2",
        "CreateProjectV2Input",
        json!({"ownerId":owners["viewer"]["id"],"title":"CLI project","clientMutationId":"create"}),
    )
    .await;
    assert_eq!(project["items"]["totalCount"], 0);
    // Two stored fields (Title, Status) and the six built-in fields.
    assert_eq!(project["fields"]["totalCount"], 8);
    let pid = project["id"].clone();
    let updated = project_mutation(&router,token,"updateProjectV2","UpdateProjectV2Input",json!({"projectId":pid,"title":"Edited","readme":"# Read me","shortDescription":"A plan","public":true,"closed":true})).await;
    assert_eq!(updated["readme"], "# Read me");
    assert_eq!(updated["closed"], true);
    let template = project_mutation(
        &router,
        token,
        "markProjectV2AsTemplate",
        "MarkProjectV2AsTemplateInput",
        json!({"projectId":pid}),
    )
    .await;
    assert_eq!(template["template"], true);
    let template = project_mutation(
        &router,
        token,
        "unmarkProjectV2AsTemplate",
        "UnmarkProjectV2AsTemplateInput",
        json!({"projectId":pid}),
    )
    .await;
    assert_eq!(template["template"], false);
    let list = query(&router,token,&format!("{{ viewer {{ projectsV2(first:1) {{ totalCount nodes {{ ...ProjectCLI }} pageInfo {{ hasNextPage endCursor }} }} projectV2(number:1) {{ ...ProjectCLI }} }} }} {PROJECT_SELECTION}"),json!({})).await;
    assert_eq!(list["viewer"]["projectsV2"]["totalCount"], 1);
    assert_eq!(list["viewer"]["projectV2"]["title"], "Edited");
    let mut fields = Vec::new();
    for (name, ty, options) in [
        ("Estimate", "NUMBER", Value::Null),
        ("Due", "DATE", Value::Null),
        ("Notes", "TEXT", Value::Null),
        (
            "Priority",
            "SINGLE_SELECT",
            json!([{"name":"High","color":"RED","description":"Urgent"}]),
        ),
    ] {
        let input =
            json!({"projectId":pid,"name":name,"dataType":ty,"singleSelectOptions":options});
        let result = query(&router,token,&format!("mutation($input:CreateProjectV2FieldInput!) {{ createProjectV2Field(input:$input) {{ projectV2Field {{ ...FieldCLI }} }} }} {FIELD_SELECTION}"),json!({"input":input})).await;
        fields.push(result["createProjectV2Field"]["projectV2Field"].clone());
    }
    let field_list = query(&router,token,&format!("{{ viewer {{ projectV2(number:1) {{ fields(first:2) {{ totalCount nodes {{ ...FieldCLI }} pageInfo {{ hasNextPage endCursor }} }} }} }} }} {FIELD_SELECTION}"),json!({})).await;
    assert_eq!(
        field_list["viewer"]["projectV2"]["fields"]["totalCount"],
        12
    );
    assert_eq!(
        field_list["viewer"]["projectV2"]["fields"]["pageInfo"]["hasNextPage"],
        true
    );
    let cursor = field_list["viewer"]["projectV2"]["fields"]["pageInfo"]["endCursor"].clone();
    let page = query(&router,token,"query($after:String) { viewer { projectV2(number:1) { fields(first:100,after:$after) { totalCount nodes { __typename } } } } }",json!({"after":cursor})).await;
    assert_eq!(
        page["viewer"]["projectV2"]["fields"]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        10
    );
    let draft = query(&router,token,&format!("mutation($input:AddProjectV2DraftIssueInput!) {{ addProjectV2DraftIssue(input:$input) {{ projectItem {{ ...ItemCLI }} }} }} {ITEM_SELECTION} {FIELD_SELECTION}"),json!({"input":{"projectId":pid,"title":"Draft","body":"Body"}})).await;
    let item = draft["addProjectV2DraftIssue"]["projectItem"]["id"].clone();
    let draft_id = draft["addProjectV2DraftIssue"]["projectItem"]["content"]["id"].clone();
    let edited = query(&router,token,"mutation($input:UpdateProjectV2DraftIssueInput!) { updateProjectV2DraftIssue(input:$input) { draftIssue { id title body } } }",json!({"input":{"draftIssueId":draft_id,"title":"New draft","body":"New body"}})).await;
    assert_eq!(
        edited["updateProjectV2DraftIssue"]["draftIssue"]["body"],
        "New body"
    );
    for (f, value) in fields.iter().zip([
        json!({"number":8.5}),
        json!({"date":"2026-10-24"}),
        json!({"text":"Notes"}),
        json!({"singleSelectOptionId":fields[3]["options"][0]["id"]}),
    ]) {
        let updated = query(&router,token,&format!("mutation($input:UpdateProjectV2ItemFieldValueInput!) {{ updateProjectV2ItemFieldValue(input:$input) {{ projectV2Item {{ ...ItemCLI }} }} }} {ITEM_SELECTION} {FIELD_SELECTION}"),json!({"input":{"projectId":pid,"itemId":item,"fieldId":f["id"],"value":value}})).await;
        assert_eq!(
            updated["updateProjectV2ItemFieldValue"]["projectV2Item"]["id"],
            item
        );
    }
    let listed = query(&router,token,&format!("query($id:ID!) {{ node(id:$id) {{ ... on ProjectV2 {{ items(first:100,query:\"New\") {{ nodes {{ ...ItemCLI }} totalCount }} }} }} }} {ITEM_SELECTION} {FIELD_SELECTION}"),json!({"id":pid})).await;
    assert_eq!(listed["node"]["items"]["totalCount"], 1);
    assert_eq!(
        listed["node"]["items"]["nodes"][0]["fieldValues"]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    for (mutation, ty, archived) in [
        ("archiveProjectV2Item", "ArchiveProjectV2ItemInput", true),
        (
            "unarchiveProjectV2Item",
            "UnarchiveProjectV2ItemInput",
            false,
        ),
    ] {
        let result = query(&router,token,&format!("mutation($input:{ty}!) {{ {mutation}(input:$input) {{ item {{ id isArchived }} }} }}"),json!({"input":{"projectId":pid,"itemId":item}})).await;
        assert_eq!(result[mutation]["item"]["isArchived"], archived);
        let result = query(
            &router,
            token,
            "{ viewer { projectV2(number:1) { items(first:0) { totalCount } } } }",
            json!({}),
        )
        .await;
        assert_eq!(
            result["viewer"]["projectV2"]["items"]["totalCount"],
            if archived { 0 } else { 1 }
        );
    }
    let cleared = query(&router,token,&format!("mutation($input:ClearProjectV2ItemFieldValueInput!) {{ clearProjectV2ItemFieldValue(input:$input) {{ projectV2Item {{ ...ItemCLI }} }} }} {ITEM_SELECTION} {FIELD_SELECTION}"),json!({"input":{"projectId":pid,"itemId":item,"fieldId":fields[0]["id"]}})).await;
    assert_eq!(
        cleared["clearProjectV2ItemFieldValue"]["projectV2Item"]["fieldValues"]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let copied = project_mutation(&router,token,"copyProjectV2","CopyProjectV2Input",json!({"projectId":pid,"ownerId":owners["viewer"]["id"],"title":"Copy","includeDraftIssues":true})).await;
    assert_eq!(copied["items"]["totalCount"], 1);
    assert_eq!(copied["fields"]["totalCount"], 12);
    assert_eq!(copied["public"], false);
    for credential in [&credentials[1], &credentials[2]] {
        let (_,denied) = call(&router,Some(credential),"mutation($input:UpdateProjectV2Input!) { updateProjectV2(input:$input) { projectV2 { id } } }",json!({"input":{"projectId":pid,"title":"Denied"}})).await;
        assert!(denied.get("errors").is_some(), "{denied}");
    }
    let (_, hidden) = call(
        &router,
        Some(&credentials[2]),
        "query($id:ID!) { node(id:$id) { id } }",
        json!({"id":copied["id"]}),
    )
    .await;
    assert!(hidden.get("errors").is_some(), "{hidden}");
    let deleted_field = query(&router,token,&format!("mutation($input:DeleteProjectV2FieldInput!) {{ deleteProjectV2Field(input:$input) {{ projectV2Field {{ ...FieldCLI }} }} }} {FIELD_SELECTION}"),json!({"input":{"fieldId":fields[0]["id"]}})).await;
    assert_eq!(
        deleted_field["deleteProjectV2Field"]["projectV2Field"]["name"],
        "Estimate"
    );
    let deleted_item = query(&router,token,"mutation($input:DeleteProjectV2ItemInput!) { deleteProjectV2Item(input:$input) { deletedItemId } }",json!({"input":{"projectId":pid,"itemId":item}})).await;
    assert_eq!(deleted_item["deleteProjectV2Item"]["deletedItemId"], item);
    let deleted = project_mutation(
        &router,
        token,
        "deleteProjectV2",
        "DeleteProjectV2Input",
        json!({"projectId":pid}),
    )
    .await;
    assert_eq!(deleted["title"], "Edited");
    assert_eq!(deleted["fields"]["totalCount"], 11);
}

#[tokio::test]
async fn graphql_projects_sqlite() {
    projects_contract(false).await;
}
#[tokio::test]
async fn graphql_projects_postgres() {
    projects_contract(true).await;
}

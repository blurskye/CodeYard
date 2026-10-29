//! gh discussion's queries and writes through the real router on both engines.
use super::*;
use codeyard::backend::repositories::lifecycle;

const META: &str = "query RepositoryMetaForDiscussions($name:String!$owner:String!){repository(owner: $owner, name: $name){id,databaseId,hasDiscussionsEnabled}}";
const LABELS: &str = "query RepositoryLabelsForDiscussions($endCursor:String$name:String!$owner:String!){repository(owner: $owner, name: $name){labels(first: 100, after: $endCursor, orderBy: {field: NAME, direction: ASC}){nodes{id,name,color},pageInfo{hasNextPage,endCursor}}}}";

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("discussion-alice").await;
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
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let session = Client::login(&router, f.pool(), alice).await;
    let token = tokens::create(
        f.pool(),
        alice,
        Some(&session.session),
        TokenInput {
            name: "discussion",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap()
    .value;
    let vars = json!({"owner":"discussion-alice","name":"proj","endCursor":null});
    let data = query(&router, &token, META, vars.clone()).await;
    assert_eq!(data["repository"].as_object().unwrap().len(), 3);
    assert!(data["repository"]["id"].is_string());
    assert_eq!(data["repository"]["databaseId"], repo.id);
    assert_eq!(data["repository"]["hasDiscussionsEnabled"], false);
    let label = db::labels::create_label(f.pool(), alice, repo.id, "help", "abcdef", None)
        .await
        .unwrap();
    let data = query(&router, &token, LABELS, vars).await;
    let connection = &data["repository"]["labels"];
    assert_eq!(connection["nodes"].as_array().unwrap().len(), 1);
    let node = &connection["nodes"][0];
    assert_eq!(node.as_object().unwrap().len(), 3);
    assert_eq!(node["name"], label.name);
    assert_eq!(node["color"], "abcdef");
    assert!(node["id"].is_string());
    assert_eq!(connection["pageInfo"]["hasNextPage"], false);
    assert!(connection["pageInfo"]["endCursor"].is_string());
    discussions(
        &mut f,
        &router,
        &token,
        alice,
        repo.id,
        &data["repository"]["labels"]["nodes"][0]["id"],
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn sqlite_gh_discussion_prerequisites() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_gh_discussion_prerequisites() {
    contract(true).await;
}

const FRAGMENTS: &str = r#"
fragment A on Actor { __typename login ...on User{id name} ...on Bot{id} }
fragment D on Discussion {
    id number title body url closed stateReason author{...A}
    category{id name slug emoji isAnswerable} labels(first:20){nodes{id name color}}
    isAnswered answerChosenAt answerChosenBy{...A}
    reactionGroups{content users{totalCount}} createdAt updatedAt closedAt locked
}
"#;
const COMMENT: &str = r#"id url author{__typename login ...on User{id name} ...on Bot{id}} body createdAt isAnswer upvoteCount reactionGroups{content users{totalCount}}"#;

async fn discussion_query(router: &Router, token: &str, text: &str, vars: Value) -> Value {
    query(router, token, &format!("{text}\n{FRAGMENTS}"), vars).await
}
async fn mutation(
    router: &Router,
    token: &str,
    field: &str,
    input_type: &str,
    input: Value,
    selection: &str,
) -> Value {
    let text = format!("mutation($input:{input_type}!){{{field}(input:$input){{{selection}}}}}");
    if selection.contains("...D") {
        discussion_query(router, token, &text, json!({"input":input})).await[field].clone()
    } else {
        query(router, token, &text, json!({"input":input})).await[field].clone()
    }
}
async fn credentials(f: &Database, router: &Router, actor: i64, scope: &str) -> String {
    let session = Client::login(router, f.pool(), actor).await;
    tokens::create(
        f.pool(),
        actor,
        Some(&session.session),
        TokenInput {
            name: scope,
            scope,
            expires_at: None,
        },
    )
    .await
    .unwrap()
    .value
}
async fn refused(router: &Router, token: &str, text: &str, vars: Value) {
    let (status, body) = call(router, Some(token), text, vars).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["errors"].is_array(), "{body}");
}

async fn discussions(
    f: &mut Database,
    router: &Router,
    token: &str,
    alice: i64,
    repo: i64,
    label: &Value,
) {
    use db::models::AccessLevel;
    let bob = f.actor("discussion-bob").await;
    let bob_token = credentials(f, router, bob, "write").await;
    let read_token = credentials(f, router, alice, "read").await;
    let vars = json!({"owner":"discussion-alice","name":"proj"});
    let meta = query(router, token, META, vars.clone()).await;
    let repo_id = meta["repository"]["id"].clone();
    let categories=query(router,token,r#"query($owner:String!,$name:String!){repository(owner:$owner,name:$name){hasDiscussionsEnabled discussionCategories(first:100){nodes{id name slug emoji isAnswerable description}}}}"#,vars.clone()).await;
    let categories = categories["repository"]["discussionCategories"]["nodes"]
        .as_array()
        .unwrap();
    assert_eq!(categories.len(), 6);
    let category = categories
        .iter()
        .find(|c| c["isAnswerable"] == true)
        .unwrap()["id"]
        .clone();
    let general = categories.iter().find(|c| c["slug"] == "general").unwrap()["id"].clone();
    let create =
        "mutation($input:CreateDiscussionInput!){createDiscussion(input:$input){discussion{id}}}";
    let new = json!({"repositoryId":repo_id,"categoryId":category,"title":"First question","body":"How does this work?","clientMutationId":"create"});
    refused(router, token, create, json!({"input":new})).await;
    let setting="mutation($input:UpdateRepositoryInput!){updateRepository(input:$input){repository{id hasDiscussionsEnabled}}}";
    refused(
        router,
        &bob_token,
        setting,
        json!({"input":{"repositoryId":repo_id,"hasDiscussionsEnabled":true}}),
    )
    .await;
    refused(
        router,
        &read_token,
        setting,
        json!({"input":{"repositoryId":repo_id,"hasDiscussionsEnabled":true}}),
    )
    .await;
    let enabled = query(
        router,
        token,
        setting,
        json!({"input":{"repositoryId":repo_id,"hasDiscussionsEnabled":true}}),
    )
    .await;
    assert_eq!(
        enabled["updateRepository"]["repository"]["hasDiscussionsEnabled"],
        true
    );
    let saved = mutation(
        router,
        token,
        "createDiscussion",
        "CreateDiscussionInput",
        new.clone(),
        "clientMutationId discussion{...D}",
    )
    .await;
    assert_eq!(saved["clientMutationId"], "create");
    let d = saved["discussion"].clone();
    let discussion = d["id"].clone();
    assert_eq!(d["number"], 1);
    assert_eq!(d["isAnswered"], false);
    assert_eq!(d["category"]["id"], category);
    assert_eq!(d["author"]["login"], "discussion-alice");
    assert_eq!(d["reactionGroups"], json!([]));
    assert!(d["url"]
        .as_str()
        .unwrap()
        .ends_with("/discussion-alice/proj/discussions/1"));
    // Discussions and issues allocate from the same sequence, including after deletion.
    let issue = mutation(
        router,
        token,
        "createIssue",
        "CreateIssueInput",
        json!({"repositoryId":repo_id,"title":"Shared sequence"}),
        "issue{number}",
    )
    .await;
    assert_eq!(issue["issue"]["number"], 2);
    let second=mutation(router,token,"createDiscussion","CreateDiscussionInput",json!({"repositoryId":repo_id,"categoryId":general,"title":"Second post","body":"Another body"}),"discussion{...D}").await["discussion"].clone();
    assert_eq!(second["number"], 3);
    refused(router, &read_token, create, json!({"input":new})).await;
    refused(router, &bob_token, create, json!({"input":new})).await;
    db::repositories::access::grant_repo_access(f.pool(), bob, repo, AccessLevel::Read)
        .await
        .unwrap();
    let edit =
        "mutation($input:UpdateDiscussionInput!){updateDiscussion(input:$input){discussion{id}}}";
    refused(
        router,
        &bob_token,
        edit,
        json!({"input":{"discussionId":discussion,"title":"Not mine"}}),
    )
    .await;
    let edited = mutation(
        router,
        token,
        "updateDiscussion",
        "UpdateDiscussionInput",
        json!({"discussionId":discussion,"title":"Edited question","body":"Updated text"}),
        "discussion{...D}",
    )
    .await;
    assert_eq!(edited["discussion"]["title"], "Edited question");
    let minimal=discussion_query(router,token,"query($owner:String!,$name:String!){repository(owner:$owner,name:$name){discussion(number:1){...D comments{totalCount}}}}",vars.clone()).await;
    assert_eq!(
        minimal["repository"]["discussion"]["comments"]["totalCount"],
        0
    );
    let add="mutation($input:AddDiscussionCommentInput!){addDiscussionComment(input:$input){comment{id}}}";
    refused(
        router,
        &bob_token,
        add,
        json!({"input":{"discussionId":discussion,"body":"Reader cannot comment"}}),
    )
    .await;
    db::repositories::access::grant_repo_access(f.pool(), bob, repo, AccessLevel::Write)
        .await
        .unwrap();
    let c = mutation(
        router,
        &bob_token,
        "addDiscussionComment",
        "AddDiscussionCommentInput",
        json!({"discussionId":discussion,"body":"A possible answer"}),
        &format!("comment{{{COMMENT}}}"),
    )
    .await["comment"]
        .clone();
    let comment = c["id"].clone();
    assert_eq!(c["author"]["login"], "discussion-bob");
    assert_eq!(c["isAnswer"], false);
    assert_eq!(c["upvoteCount"], 0);
    let reply = mutation(
        router,
        token,
        "addDiscussionComment",
        "AddDiscussionCommentInput",
        json!({"discussionId":discussion,"body":"Thanks","replyToId":comment}),
        &format!("comment{{{COMMENT}}}"),
    )
    .await["comment"]
        .clone();
    refused(
        router,
        token,
        add,
        json!({"input":{"discussionId":discussion,"body":"Too deep","replyToId":reply["id"]}}),
    )
    .await;
    refused(
        router,
        token,
        add,
        json!({"input":{"discussionId":second["id"],"body":"Wrong thread","replyToId":comment}}),
    )
    .await;
    refused(
        router,
        &read_token,
        add,
        json!({"input":{"discussionId":discussion,"body":"Read token"}}),
    )
    .await;
    let update_comment="mutation($input:UpdateDiscussionCommentInput!){updateDiscussionComment(input:$input){comment{id}}}";
    let charlie = f.actor("discussion-charlie").await;
    let charlie_token = credentials(f, router, charlie, "write").await;
    db::repositories::access::grant_repo_access(f.pool(), charlie, repo, AccessLevel::Read)
        .await
        .unwrap();
    refused(
        router,
        &charlie_token,
        update_comment,
        json!({"input":{"commentId":comment,"body":"Not mine"}}),
    )
    .await;
    let updated = mutation(
        router,
        &bob_token,
        "updateDiscussionComment",
        "UpdateDiscussionCommentInput",
        json!({"commentId":comment,"body":"The answer, edited"}),
        &format!("comment{{{COMMENT}}}"),
    )
    .await;
    assert_eq!(updated["comment"]["body"], "The answer, edited");
    let view_query = r#"query($owner:String!,$name:String!){
        repository(owner:$owner,name:$name){discussion(number:1){...D
            comments(first:1){totalCount pageInfo{endCursor hasNextPage startCursor hasPreviousPage}
                nodes{COMMENT_FIELDS replies(last:4){totalCount nodes{COMMENT_FIELDS}}}
            }
        }}
    }"#
    .replace("COMMENT_FIELDS", COMMENT);
    let view = discussion_query(router, token, &view_query, vars.clone()).await;
    let comments = &view["repository"]["discussion"]["comments"];
    assert_eq!(comments["totalCount"], 1);
    assert_eq!(comments["nodes"][0]["replies"]["totalCount"], 1);
    assert_eq!(
        comments["nodes"][0]["replies"]["nodes"][0]["body"],
        "Thanks"
    );
    let node_query = r#"query($id:ID!){node(id:$id){__typename ...on DiscussionComment{
        COMMENT_FIELDS discussion{...D}
        replies(last:1){totalCount pageInfo{endCursor hasNextPage startCursor hasPreviousPage} nodes{COMMENT_FIELDS}}
    }}}"#.replace("COMMENT_FIELDS", COMMENT);
    let node = discussion_query(router, token, &node_query, json!({"id":comment})).await;
    assert_eq!(node["node"]["discussion"]["id"], discussion);
    assert_eq!(node["node"]["replies"]["nodes"][0]["id"], reply["id"]);
    let direct=query(router,token,&format!("query($id:ID!){{node(id:$id){{__typename ...on DiscussionComment{{{COMMENT} discussion{{id}}}}}}}}"),json!({"id":comment})).await;
    assert_eq!(direct["node"]["__typename"], "DiscussionComment");
    // gh's numeric comment URL path emits compact MessagePack rather than our full-width encoding.
    let stored = db::repository_discussions::by_number(f.pool(), repo, 1)
        .await
        .unwrap()
        .unwrap();
    let (_, stored_comments) =
        db::repository_discussions::comments(f.pool(), stored.id, None, 0, 100)
            .await
            .unwrap();
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let mut packed = vec![0x93, 0, 0xcf];
    packed.extend_from_slice(&repo.to_be_bytes());
    packed.push(0xce);
    packed.extend_from_slice(&(stored_comments[0].id as u32).to_be_bytes());
    let numeric = format!("DC_{}", URL_SAFE_NO_PAD.encode(packed));
    let numeric_node = query(
        router,
        token,
        "query($id:ID!){node(id:$id){...on DiscussionComment{body}}}",
        json!({"id":numeric}),
    )
    .await;
    assert_eq!(numeric_node["node"]["body"], "The answer, edited");
    mutation(
        router,
        token,
        "addDiscussionComment",
        "AddDiscussionCommentInput",
        json!({"discussionId":discussion,"body":"Another comment"}),
        "comment{id}",
    )
    .await;
    let window = "query($id:ID!,$first:Int,$last:Int,$after:String,$before:String){node(id:$id){...on Discussion{comments(first:$first,last:$last,after:$after,before:$before){totalCount pageInfo{endCursor startCursor hasNextPage hasPreviousPage} nodes{body}}}}}";
    let oldest = query(router, token, window, json!({"id":discussion,"first":1})).await;
    let page = &oldest["node"]["comments"];
    assert_eq!(page["totalCount"], 2);
    assert_eq!(page["pageInfo"]["hasNextPage"], true);
    let newest = query(
        router,
        token,
        window,
        json!({"id":discussion,"last":1,"after":page["pageInfo"]["endCursor"]}),
    )
    .await;
    assert_eq!(
        newest["node"]["comments"]["nodes"][0]["body"],
        "Another comment"
    );
    let backward=query(router,token,window,json!({"id":discussion,"last":1,"before":newest["node"]["comments"]["pageInfo"]["startCursor"]})).await;
    assert_eq!(
        backward["node"]["comments"]["nodes"][0]["body"],
        "The answer, edited"
    );
    let answer="mutation($input:MarkDiscussionCommentAsAnswerInput!){markDiscussionCommentAsAnswer(input:$input){discussion{id}}}";
    refused(router, &bob_token, answer, json!({"input":{"id":comment}})).await;
    let marked = mutation(
        router,
        token,
        "markDiscussionCommentAsAnswer",
        "MarkDiscussionCommentAsAnswerInput",
        json!({"id":comment}),
        "discussion{...D answer{id isAnswer}}",
    )
    .await;
    assert_eq!(marked["discussion"]["isAnswered"], true);
    assert_eq!(marked["discussion"]["answer"]["isAnswer"], true);
    assert!(marked["discussion"]["answerChosenAt"].is_string());
    assert_eq!(
        marked["discussion"]["answerChosenBy"]["login"],
        "discussion-alice"
    );
    let list_query="query($owner:String!,$name:String!,$after:String,$answered:Boolean,$category:ID,$states:[DiscussionState!]){repository(owner:$owner,name:$name){hasDiscussionsEnabled discussions(first:1,after:$after,answered:$answered,categoryId:$category,states:$states,orderBy:{field:CREATED_AT,direction:ASC}){totalCount pageInfo{endCursor hasNextPage} nodes{...D}}}}";
    let listed = discussion_query(router, token, list_query, vars.clone()).await;
    let page = &listed["repository"]["discussions"];
    assert_eq!(page["totalCount"], 2);
    assert_eq!(page["nodes"][0]["id"], discussion);
    assert_eq!(page["pageInfo"]["hasNextPage"], true);
    let next = discussion_query(
        router,
        token,
        list_query,
        json!({"owner":"discussion-alice","name":"proj","after":page["pageInfo"]["endCursor"]}),
    )
    .await;
    assert_eq!(
        next["repository"]["discussions"]["nodes"][0]["id"],
        second["id"]
    );
    let filtered=discussion_query(router,token,list_query,json!({"owner":"discussion-alice","name":"proj","answered":true,"category":category,"states":["OPEN"]})).await;
    assert_eq!(filtered["repository"]["discussions"]["totalCount"], 1);
    let closed = discussion_query(
        router,
        token,
        list_query,
        json!({"owner":"discussion-alice","name":"proj","states":["CLOSED"]}),
    )
    .await;
    assert_eq!(closed["repository"]["discussions"]["totalCount"], 0);
    let unmarked = mutation(
        router,
        token,
        "unmarkDiscussionCommentAsAnswer",
        "UnmarkDiscussionCommentAsAnswerInput",
        json!({"id":comment}),
        "discussion{...D answer{id}}",
    )
    .await;
    assert_eq!(unmarked["discussion"]["isAnswered"], false);
    assert!(unmarked["discussion"]["answer"].is_null());
    // Labels are repository-scoped and require triage, not just read access.
    db::repositories::access::grant_repo_access(f.pool(), bob, repo, AccessLevel::Read)
        .await
        .unwrap();
    let labels_input = json!({"labelableId":discussion,"labelIds":[label]});
    refused(router,&bob_token,"mutation($input:AddLabelsToLabelableInput!){addLabelsToLabelable(input:$input){labelable{...on Discussion{id}}}}",json!({"input":labels_input})).await;
    db::repositories::access::grant_repo_access(f.pool(), bob, repo, AccessLevel::Triage)
        .await
        .unwrap();
    let labelled = mutation(
        router,
        &bob_token,
        "addLabelsToLabelable",
        "AddLabelsToLabelableInput",
        labels_input.clone(),
        "labelable{...on Discussion{...D}}",
    )
    .await;
    assert_eq!(labelled["labelable"]["labels"]["nodes"][0]["id"], *label);
    let search="query($query:String!){search(type:DISCUSSION,query:$query,first:10){discussionCount pageInfo{hasNextPage endCursor} nodes{...on Discussion{...D}}}}";
    for expression in ["repo:discussion-alice/proj author:\"discussion-alice\" label:\"help\" sort:updated-desc Edited", "repo:discussion-alice/proj is:unanswered category:\"Q&A\" sort:created-asc"] {
        let found=discussion_query(router,token,search,json!({"query":expression})).await;
        assert_eq!(found["search"]["discussionCount"],1,"{found}");
        assert_eq!(found["search"]["nodes"][0]["id"],discussion);
    }
    let removed = mutation(
        router,
        token,
        "removeLabelsFromLabelable",
        "RemoveLabelsFromLabelableInput",
        labels_input,
        "labelable{...on Discussion{...D}}",
    )
    .await;
    assert_eq!(removed["labelable"]["labels"]["nodes"], json!([]));
    let triaged = mutation(
        router,
        &bob_token,
        "updateDiscussion",
        "UpdateDiscussionInput",
        json!({"discussionId":discussion,"categoryId":general}),
        "discussion{...D}",
    )
    .await;
    assert_eq!(triaged["discussion"]["category"]["id"], general);
    refused(router, token, answer, json!({"input":{"id":comment}})).await;
    // Locking is enforced at the write, not merely reflected in presentation.
    f.execute(
        "UPDATE discussions SET locked=true WHERE id=$1",
        &[stored.id],
    )
    .await;
    refused(
        router,
        token,
        add,
        json!({"input":{"discussionId":discussion,"body":"Locked"}}),
    )
    .await;
    f.execute(
        "UPDATE discussions SET locked=false WHERE id=$1",
        &[stored.id],
    )
    .await;
    // The author keeps edit/delete authority with only read access.
    db::repositories::access::grant_repo_access(f.pool(), bob, repo, AccessLevel::Read)
        .await
        .unwrap();
    let own = mutation(
        router,
        &bob_token,
        "updateDiscussionComment",
        "UpdateDiscussionCommentInput",
        json!({"commentId":comment,"body":"Author edit"}),
        "comment{body}",
    )
    .await;
    assert_eq!(own["comment"]["body"], "Author edit");
    let deleted = mutation(
        router,
        &bob_token,
        "deleteDiscussionComment",
        "DeleteDiscussionCommentInput",
        json!({"id":comment}),
        "comment{id}",
    )
    .await;
    assert_eq!(deleted["comment"]["id"], comment);
    refused(
        router,
        token,
        "query($id:ID!){node(id:$id){id}}",
        json!({"id":reply["id"]}),
    )
    .await;
    let deleted = mutation(
        router,
        token,
        "deleteDiscussion",
        "DeleteDiscussionInput",
        json!({"id":discussion}),
        "discussion{id}",
    )
    .await;
    assert_eq!(deleted["discussion"]["id"], discussion);
    let missing=query(router,token,"query($owner:String!,$name:String!){repository(owner:$owner,name:$name){discussion(number:1){id}}}",vars.clone()).await;
    assert!(missing["repository"]["discussion"].is_null());
    // Private node and search reads repeat repository authority.
    f.execute(
        "UPDATE repositories SET is_private=true WHERE id=$1",
        &[repo],
    )
    .await;
    let stranger = f.actor("discussion-stranger").await;
    let stranger_token = credentials(f, router, stranger, "write").await;
    refused(
        router,
        &stranger_token,
        "query($id:ID!){node(id:$id){id}}",
        json!({"id":second["id"]}),
    )
    .await;
    let hidden = discussion_query(
        router,
        &stranger_token,
        search,
        json!({"query":"repo:discussion-alice/proj"}),
    )
    .await;
    assert_eq!(hidden["search"]["discussionCount"], 0);
    query(
        router,
        token,
        setting,
        json!({"input":{"repositoryId":repo_id,"hasDiscussionsEnabled":false}}),
    )
    .await;
    refused(router, token, create, json!({"input":new})).await;
}

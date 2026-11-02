use super::{call, json, query, request};
use axum::http::StatusCode;
use axum::Router;
pub(super) async fn exercise(router: &Router, tokens: (&str, &str), repository: serde_json::Value) {
    let (owner, reader) = tokens;
    let input = json!({"name":"graphql-created-repository","visibility":"PRIVATE","hasIssuesEnabled":true,"hasWikiEnabled":false,"homepageUrl":"https://example.test/created","description":"CLI repository"});
    let created = query(
        router,
        owner,
        include_str!("gh-repository-create.graphql"),
        json!({"input":input}),
    )
    .await;
    let id = created["createRepository"]["repository"]["id"].clone();
    assert_eq!(
        created["createRepository"]["repository"]["owner"]["login"],
        "graphql-alice"
    );
    let updated=query(router,owner,"mutation($input:UpdateRepositoryInput!){updateRepository(input:$input){repository{id name description homepageUrl hasWikiEnabled isTemplate hasDiscussionsEnabled discussionCategories(first:10){totalCount}}}}",json!({"input":{"repositoryId":id,"name":"graphql-edited-repository","description":"Edited CLI repository","homepageUrl":"https://example.test/edited","hasIssuesEnabled":true,"hasWikiEnabled":true,"template":true,"hasDiscussionsEnabled":true}})).await;
    assert_eq!(
        updated["updateRepository"]["repository"]["hasWikiEnabled"],
        true
    );
    assert_eq!(
        updated["updateRepository"]["repository"]["isTemplate"],
        true
    );
    assert_eq!(
        updated["updateRepository"]["repository"]["hasDiscussionsEnabled"],
        true
    );
    assert!(
        updated["updateRepository"]["repository"]["discussionCategories"]["totalCount"]
            .as_i64()
            .unwrap()
            > 0
    );
    assert_eq!(
        updated["updateRepository"]["repository"]["description"],
        "Edited CLI repository"
    );
    assert_eq!(
        updated["updateRepository"]["repository"]["homepageUrl"],
        "https://example.test/edited"
    );
    let (status, rest) = request(
        router,
        Some(owner),
        "GET",
        "/api/v3/repos/graphql-alice/graphql-edited-repository",
        json!(null),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rest}");
    assert_eq!(rest["node_id"], id);
    // As on GitHub, a repository may be created with issues turned off.
    let mut no_issues = input.clone();
    no_issues["name"] = json!("no-issues");
    no_issues["hasIssuesEnabled"] = json!(false);
    query(
        router,
        owner,
        include_str!("gh-repository-create.graphql"),
        json!({"input":no_issues}),
    )
    .await;
    let (status, rest) = request(
        router,
        Some(owner),
        "GET",
        "/api/v3/repos/graphql-alice/no-issues",
        json!(null),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rest}");
    assert_eq!(rest["has_issues"], false, "{rest}");
    let org = query(
        router,
        owner,
        "{organization(login:\"graphql-org\"){id team(slug:\"graphql-team\"){id}}}",
        json!({}),
    )
    .await;
    let org_input = json!({"name":"graphql-org-created","visibility":"PRIVATE","hasWikiEnabled":false,"hasIssuesEnabled":true,"ownerId":org["organization"]["id"],"teamId":org["organization"]["team"]["id"]});
    let (_, denied) = call(
        router,
        Some(reader),
        include_str!("gh-repository-create.graphql"),
        json!({"input":org_input}),
    )
    .await;
    assert!(denied["errors"].is_array());
    let created = query(
        router,
        owner,
        include_str!("gh-repository-create.graphql"),
        json!({"input":org_input}),
    )
    .await;
    assert_eq!(
        created["createRepository"]["repository"]["owner"]["login"],
        "graphql-org"
    );
    let (_, denied) = call(
        router,
        Some(reader),
        "mutation($input:ArchiveRepositoryInput!){archiveRepository(input:$input){repository{id}}}",
        json!({"input":{"repositoryId":repository}}),
    )
    .await;
    assert!(denied["errors"].is_array(), "{denied}");
    for (field, input, archived) in [
        ("archiveRepository", "ArchiveRepositoryInput", true),
        ("unarchiveRepository", "UnarchiveRepositoryInput", false),
    ] {
        let document = format!(
            "mutation($input:{input}!){{{field}(input:$input){{repository{{id isArchived}}}}}}"
        );
        let value = query(
            router,
            owner,
            &document,
            json!({"input":{"repositoryId":repository}}),
        )
        .await;
        assert_eq!(value[field]["repository"]["isArchived"], archived);
    }
}

use super::applied;
use super::call;
use super::json;
use super::query;
use super::request;
use axum::{http::StatusCode, Router};
use codeyard_app::app::AppState;
use codeyard_repos_core::repositories::lifecycle;
pub(super) async fn exercise(
    state: &AppState,
    router: &Router,
    tokens: (&str, &str),
    scope: (i64, i64),
    oid: &str,
) {
    let (alice, bob) = tokens;
    let (actor, org) = scope;
    let (status, comment) = request(
        router,
        Some(alice),
        "POST",
        &format!("/api/v3/repos/graphql-alice/proj/commits/{oid}/comments"),
        json!({"body":"Commit discussion"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{comment}");
    let id = comment["node_id"].clone();
    let view=query(router,alice,"query($id:ID!){node(id:$id){id __typename ...on CommitComment{body author{login} authorAssociation commit{oid} repository{name} viewerDidAuthor}}}",json!({"id":id})).await;
    assert_eq!(view["node"]["__typename"], "CommitComment");
    assert_eq!(view["node"]["body"], "Commit discussion");
    assert_eq!(view["node"]["authorAssociation"], "OWNER");
    assert_eq!(view["node"]["commit"]["oid"], oid);
    let (_, hidden) = call(
        router,
        Some(bob),
        "query($id:ID!){node(id:$id){id}}",
        json!({"id":id}),
    )
    .await;
    assert_eq!(hidden["data"]["node"], serde_json::Value::Null);
    assert!(hidden["errors"].is_array());
    let listed=query(router,alice,"query($oid:GitObjectID!){repository(owner:\"graphql-alice\",name:\"proj\"){object(oid:$oid){...on Commit{comments(first:10){totalCount nodes{id body}}}}}}",json!({"oid":oid})).await;
    assert_eq!(listed["repository"]["object"]["comments"]["totalCount"], 1);
    assert_eq!(
        listed["repository"]["object"]["comments"]["nodes"][0]["id"],
        id
    );
    let path = format!(
        "/api/v3/repos/graphql-alice/proj/comments/{}",
        comment["id"].as_i64().unwrap()
    );
    let (status, edited) = request(
        router,
        Some(alice),
        "PATCH",
        &path,
        json!({"body":"Edited commit discussion"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    let view = query(
        router,
        alice,
        "query($id:ID!){node(id:$id){...on CommitComment{body}}}",
        json!({"id":id}),
    )
    .await;
    assert_eq!(view["node"]["body"], "Edited commit discussion");
    let (status, _) = request(router, Some(alice), "DELETE", &path, json!(null)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, gone) = call(
        router,
        Some(alice),
        "query($id:ID!){node(id:$id){id}}",
        json!({"id":id}),
    )
    .await;
    assert_eq!(gone["data"]["node"], serde_json::Value::Null);
    let team = applied(
        codeyard_db_orgs::organizations::teams::create_organization_team(
            &state.pool,
            actor,
            org,
            &codeyard_db_orgs::organizations::teams::NewTeam::new(
                "graphql-team",
                codeyard_db_orgs::organizations::teams::TeamProfile::new(
                    "GraphQL team",
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
    let repo = lifecycle::provision(
        (&state.pool, None),
        &state.repos_path,
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner {
            actor_id: actor,
            target: codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(org),
        },
        "team-nodes",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    applied(
        codeyard_db_orgs::organizations::teams::grant_organization_team_repository(
            &state.pool,
            actor,
            org,
            team.id,
            repo.id,
            codeyard_db_orgs::organizations::teams::TeamRepositoryPermission::new(3).unwrap(),
        )
        .await
        .unwrap(),
    );
    let (status, value) = request(
        router,
        Some(alice),
        "PUT",
        "/api/v3/repos/graphql-org/team-nodes/contents/seed.txt",
        json!({"message":"Seed team repository","content":"c2VlZAo="}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    codeyard_db_repos::repositories::protection::upsert_branch_protection_rule(
        &state.pool,
        codeyard_db_repos::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: actor,
            repo_id: repo.id,
            pattern: "main",
            restrict_pushes: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (status, teams) = request(
        router,
        Some(alice),
        "PUT",
        "/api/v3/repos/graphql-org/team-nodes/branches/main/protection/restrictions/teams",
        json!(["graphql-team"]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{teams}");
    let id = teams[0]["node_id"].clone();
    assert_eq!(
        id,
        codeyard_model::node_id::scoped("T", org, team.id).unwrap()
    );
    let view=query(router,alice,"query($id:ID!){node(id:$id){id __typename ...on Team{name slug privacy organization{login}}} organization(login:\"graphql-org\"){team(slug:\"graphql-team\"){id}}}",json!({"id":id})).await;
    assert_eq!(view["node"]["__typename"], "Team");
    assert_eq!(view["node"]["slug"], "graphql-team");
    assert_eq!(view["organization"]["team"]["id"], id);
    let (_, hidden) = call(
        router,
        Some(bob),
        "query($id:ID!){node(id:$id){id}}",
        json!({"id":id}),
    )
    .await;
    assert_eq!(hidden["data"]["node"], serde_json::Value::Null);
    assert!(hidden["errors"].is_array());
}

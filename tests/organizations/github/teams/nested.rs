use super::{call, Client};
use crate::support::organizations::applied;
use axum::{http::StatusCode, Router};
use codeyard::backend::db::{self, models::AccessLevel, organizations as orgs};
use serde_json::{json, Value};

pub(super) async fn run(
    router: &Router,
    session: &Client,
    scope: (&db::DbPool, i64, i64, i64, i64),
    tokens: (&str, &str, &str),
) {
    let (pool, org, alice, charlie, repo) = scope;
    let (writer, reader, member) = tokens;
    let base = "/api/v3/orgs/team-org/teams";
    let root = call(
        router,
        "POST",
        base,
        writer,
        json!({"name":"Platform","privacy":"closed"}),
        ("teams/create", 201),
    )
    .await;
    let root_id = root["id"].as_i64().unwrap();
    let root_path = format!("{base}/platform");
    let child = call(
        router,
        "POST",
        base,
        writer,
        json!({"name":"API Engineers","parent_team_id":root_id,"parent_team_slug":"ignored-missing-slug","maintainers":["charlie"]}),
        ("teams/create", 201),
    )
    .await;
    let child_id = child["id"].as_i64().unwrap();
    let child_path = format!("{base}/api-engineers");
    assert_eq!(child["privacy"], "closed");
    assert_eq!(child["parent"]["id"], root_id);
    for (path, op) in [
        (format!("{root_path}/teams"), "teams/list-child-in-org"),
        (
            format!("/api/v3/teams/{root_id}/teams"),
            "teams/list-child-legacy",
        ),
    ] {
        let rows = call(router, "GET", &path, reader, Value::Null, (op, 200)).await;
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["id"], child_id);
    }
    let inherited = call(
        router,
        "GET",
        &format!("{root_path}/memberships/charlie"),
        reader,
        Value::Null,
        ("teams/get-membership-for-user-in-org", 200),
    )
    .await;
    assert_eq!(inherited["role"], "member");
    call(
        router,
        "DELETE",
        &format!("{root_path}/memberships/charlie"),
        writer,
        Value::Null,
        ("teams/remove-membership-for-user-in-org", 403),
    )
    .await;
    let rows = call(
        router,
        "GET",
        &format!("{root_path}/members?role=member"),
        reader,
        Value::Null,
        ("teams/list-members-in-org", 200),
    )
    .await;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["login"], "charlie");
    let root_repo = format!("{root_path}/repos/team-org/team-repo");
    let child_repo = format!("{child_path}/repos/team-org/team-repo");
    call(
        router,
        "PUT",
        &root_repo,
        writer,
        json!({"permission":"push"}),
        ("teams/add-or-update-repo-permissions-in-org", 204),
    )
    .await;
    let inherited_repo = call(
        router,
        "GET",
        &child_repo,
        reader,
        Value::Null,
        ("teams/check-permissions-for-repo-in-org", 200),
    )
    .await;
    assert_eq!(inherited_repo["permissions"]["push"], true);
    assert_eq!(inherited_repo["permissions"]["admin"], false);
    call(
        router,
        "GET",
        &child_repo,
        member,
        Value::Null,
        ("teams/check-permissions-for-repo-in-org", 200),
    )
    .await;
    let file_path = "/api/v3/repos/team-org/team-repo/contents/nested-team.txt";
    let file = call(
        router,
        "PUT",
        file_path,
        member,
        json!({"message":"Write through the parent team","content":"bmVzdGVkCg=="}),
        ("repos/create-or-update-file-contents", 201),
    )
    .await;
    call(
        router,
        "DELETE",
        &child_repo,
        writer,
        Value::Null,
        ("teams/remove-repo-in-org", 403),
    )
    .await;
    assert_eq!(
        call(
            router,
            "GET",
            &format!("{child_path}/repos"),
            reader,
            Value::Null,
            ("teams/list-repos-in-org", 200)
        )
        .await[0]["id"],
        repo
    );
    let row = db::repositories::records::get_repository_by_id(pool, repo)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        orgs::get_organization_repo_access(pool, charlie, &row)
            .await
            .unwrap(),
        AccessLevel::Write
    );
    assert_eq!(
        db::repositories::access::get_user_repo_access(pool, charlie, repo)
            .await
            .unwrap(),
        AccessLevel::Write
    );
    assert!(
        db::repositories::push_teams::contains_actor(pool, repo, charlie, &[child_id])
            .await
            .unwrap()
    );
    assert!(
        orgs::team_members_by_names(pool, org, &["team-org/platform".into()])
            .await
            .unwrap()
            .iter()
            .any(|(_, id)| *id == charlie)
    );
    assert!(db::assignees::list_assignable_usernames(pool, repo)
        .await
        .unwrap()
        .contains(&"charlie".into()));
    call(
        router,
        "PUT",
        &child_repo,
        writer,
        json!({"permission":"admin"}),
        ("teams/add-or-update-repo-permissions-in-org", 204),
    )
    .await;
    assert_eq!(
        call(
            router,
            "GET",
            &child_repo,
            reader,
            Value::Null,
            ("teams/check-permissions-for-repo-in-org", 200)
        )
        .await["permissions"]["admin"],
        true
    );
    call(
        router,
        "DELETE",
        &child_repo,
        writer,
        Value::Null,
        ("teams/remove-repo-in-org", 204),
    )
    .await;
    assert_eq!(
        db::repositories::access::get_user_repo_access(pool, charlie, repo)
            .await
            .unwrap(),
        AccessLevel::Write
    );
    for (path, input) in [
        (&child_path, json!({"privacy":"secret"})),
        (&root_path, json!({"privacy":"secret"})),
        (&root_path, json!({"parent_team_id":child_id})),
        (&child_path, json!({"parent_team_id":child_id})),
    ] {
        call(
            router,
            "PATCH",
            path,
            writer,
            input,
            ("teams/update-in-org", 422),
        )
        .await;
    }
    let foreign_org = applied(
        orgs::create_organization(
            pool,
            alice,
            &orgs::NewOrganization::new(
                "foreign-teams",
                orgs::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let foreign = call(
        router,
        "POST",
        "/api/v3/orgs/foreign-teams/teams",
        writer,
        json!({"name":"Foreign Parent","privacy":"closed"}),
        ("teams/create", 201),
    )
    .await;
    call(
        router,
        "PATCH",
        &child_path,
        writer,
        json!({"parent_team_id":foreign["id"]}),
        ("teams/update-in-org", 422),
    )
    .await;
    call(
        router,
        "DELETE",
        &format!("/api/v3/teams/{}", foreign["id"].as_i64().unwrap()),
        writer,
        Value::Null,
        ("teams/delete-legacy", 204),
    )
    .await;
    applied(
        orgs::delete_organization(pool, alice, foreign_org.id)
            .await
            .unwrap(),
    );
    let parent_id = root_id.to_string();
    let created = session
        .form_reply(
            &format!("/settings/organizations/{org}?tab=teams"),
            &[
                ("action", "team"),
                ("slug", "release-engineers"),
                ("name", "Release Engineers"),
                ("description", ""),
                ("privacy", "visible"),
                ("parent_team_id", &parent_id),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
    let sibling = call(
        router,
        "GET",
        &format!("{base}/release-engineers"),
        reader,
        Value::Null,
        ("teams/get-by-name", 200),
    )
    .await;
    assert_eq!(sibling["parent"]["id"], root_id);
    assert_eq!(
        call(
            router,
            "GET",
            &format!("{root_path}/teams?per_page=1&page=2"),
            reader,
            Value::Null,
            ("teams/list-child-in-org", 200)
        )
        .await[0]["id"],
        sibling["id"]
    );
    call(
        router,
        "PATCH",
        &child_path,
        writer,
        json!({"parent_team_id":null}),
        ("teams/update-in-org", 200),
    )
    .await;
    assert_eq!(
        db::repositories::access::get_user_repo_access(pool, charlie, repo)
            .await
            .unwrap(),
        AccessLevel::None
    );
    call(router,"PUT",file_path,member,json!({"message":"Refuse a detached team write","content":"bmVzdGVkCg==","sha":file["content"]["sha"]}),("repos/create-or-update-file-contents",404)).await;
    assert!(
        !db::repositories::push_teams::contains_actor(pool, repo, charlie, &[root_id])
            .await
            .unwrap()
    );
    call(
        router,
        "GET",
        &child_repo,
        reader,
        Value::Null,
        ("teams/check-permissions-for-repo-in-org", 404),
    )
    .await;
    let teams = call(
        router,
        "GET",
        "/api/v3/user/teams",
        member,
        Value::Null,
        ("teams/list-for-authenticated-user", 200),
    )
    .await;
    assert!(!teams
        .as_array()
        .unwrap()
        .iter()
        .any(|team| team["id"] == root_id));
    call(
        router,
        "PATCH",
        &child_path,
        writer,
        json!({"parent_team_slug":"platform"}),
        ("teams/update-in-org", 200),
    )
    .await;
    assert_eq!(
        db::repositories::access::get_user_repo_access(pool, charlie, repo)
            .await
            .unwrap(),
        AccessLevel::Write
    );
    let team_id = child_id.to_string();
    let settings = format!("/settings/organizations/{org}?tab=teams&team={child_id}");
    assert!(session.page_ok(&settings).await.contains("Parent team ID"));
    for (parent, expected) in [
        ("", AccessLevel::None),
        (parent_id.as_str(), AccessLevel::Write),
    ] {
        let reply = session
            .form_reply(
                &settings,
                &[
                    ("action", "team-edit"),
                    ("team_id", &team_id),
                    ("name", "API Engineers"),
                    ("description", ""),
                    ("privacy", "visible"),
                    ("parent_team_id", parent),
                ],
            )
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
        assert_eq!(
            db::repositories::access::get_user_repo_access(pool, charlie, repo)
                .await
                .unwrap(),
            expected
        );
    }
    call(
        router,
        "DELETE",
        &root_path,
        writer,
        Value::Null,
        ("teams/delete-in-org", 204),
    )
    .await;
    assert_eq!(
        call(
            router,
            "GET",
            &child_path,
            reader,
            Value::Null,
            ("teams/get-by-name", 200)
        )
        .await["parent"],
        Value::Null
    );
    assert_eq!(
        db::repositories::access::get_user_repo_access(pool, charlie, repo)
            .await
            .unwrap(),
        AccessLevel::None
    );
    call(
        router,
        "DELETE",
        &child_path,
        writer,
        Value::Null,
        ("teams/delete-in-org", 204),
    )
    .await;
    call(
        router,
        "DELETE",
        &format!("/api/v3/teams/{}", sibling["id"].as_i64().unwrap()),
        writer,
        Value::Null,
        ("teams/delete-legacy", 204),
    )
    .await;
}

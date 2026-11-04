//! Team REST and legacy routes use the native organization policy on both engines.
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use crate::support::organizations::applied;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_app::ci::config::CiConfig;

use codeyard_storage::ObjectStore;
use codeyard_db_core::models::OrgRole;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

mod nested;

async fn call(
    router: &Router,
    method: &str,
    path: &str,
    token: &str,
    input: Value,
    expected: (&str, u16),
) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", token)
                .header("content-type", "application/json")
                .header("accept", "application/vnd.github.v3.repository+json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status.as_u16(),
        expected.1,
        "{method} {path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    if expected.1 == 204 {
        assert!(bytes.is_empty());
        return Value::Null;
    }
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_github_schema(expected.0, expected.1, &body);
    body
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let eve = f.actor("eve").await;
    let charlie = f.actor("charlie").await;
    let org = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            f.pool(),
            alice,
            &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
                "team-org",
                codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        codeyard_db_orgs::organizations::membership::add_organization_member(
            f.pool(),
            alice,
            org.id,
            bob,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        codeyard_db_orgs::organizations::membership::add_organization_member(
            f.pool(),
            alice,
            org.id,
            charlie,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let session = Client::login(&router, f.pool(), alice).await;
    let mut tokens = Vec::new();
    for (user, name, scope) in [
        (alice, "writer", "write"),
        (alice, "reader", "read"),
        (bob, "member", "write"),
        (eve, "outsider", "write"),
        (charlie, "other-member", "write"),
    ] {
        let client = Client::login(&router, f.pool(), user).await;
        tokens.push(format!(
            "Bearer {}",
            codeyard_identity::tokens::create(
                f.pool(),
                user,
                Some(&client.session),
                TokenInput {
                    name,
                    scope,
                    expires_at: None
                }
            )
            .await
            .unwrap()
            .value
        ));
    }
    let writer = &tokens[0];
    let reader = &tokens[1];
    let member = &tokens[2];
    let outsider = &tokens[3];
    let other_member = &tokens[4];
    let base = "/api/v3/orgs/team-org/teams";
    call(
        &router,
        "POST",
        base,
        reader,
        json!({"name":"Blocked"}),
        ("teams/create", 403),
    )
    .await;
    call(
        &router,
        "POST",
        base,
        member,
        json!({"name":"Blocked"}),
        ("teams/create", 403),
    )
    .await;
    for bad in [
        json!({}),
        json!({"name":""}),
        json!({"name":"Bad","privacy":"public"}),
        json!({"name":"Bad","notification_setting":"invalid"}),
        json!({"name":"Bad","maintainers":["eve"]}),
        json!({"name":"Bad","repo_names":["alice/missing"]}),
    ] {
        call(&router, "POST", base, writer, bad, ("teams/create", 422)).await;
    }
    let created = call(&router,"POST",base,writer,json!({"name":"Release Writers","description":"Release publishers","privacy":"closed","permission":"push","notification_setting":"notifications_disabled","maintainers":["bob"]}),("teams/create",201)).await;
    let id = created["id"].as_i64().unwrap();
    assert_eq!(created["slug"], "release-writers");
    assert_eq!(created["members_count"], 2);
    assert_eq!(created["permission"], "push");
    assert_eq!(created["notification_setting"], "notifications_disabled");
    let path = format!("{base}/release-writers");
    let legacy = format!("/api/v3/teams/{id}");
    for (path, op) in [(&path, "teams/get-by-name"), (&legacy, "teams/get-legacy")] {
        assert_eq!(
            call(&router, "GET", path, reader, Value::Null, (op, 200)).await,
            created
        );
        call(&router, "GET", path, outsider, Value::Null, (op, 404)).await;
    }
    call(
        &router,
        "POST",
        base,
        writer,
        json!({"name":"Release Writers"}),
        ("teams/create", 422),
    )
    .await;
    call(
        &router,
        "PATCH",
        &path,
        reader,
        json!({"description":"blocked"}),
        ("teams/update-in-org", 403),
    )
    .await;
    let edited = call(
        &router,
        "PATCH",
        &path,
        writer,
        json!({"description":null,"privacy":"secret"}),
        ("teams/update-in-org", 200),
    )
    .await;
    assert_eq!(edited["description"], Value::Null);
    assert_eq!(edited["privacy"], "secret");
    assert_eq!(edited["permission"], "push");
    call(
        &router,
        "PATCH",
        &legacy,
        writer,
        json!({"name":"Release Writers","privacy":"closed"}),
        ("teams/update-legacy", 200),
    )
    .await;
    assert_eq!(
        call(
            &router,
            "GET",
            base,
            reader,
            Value::Null,
            ("teams/list", 200)
        )
        .await
        .as_array()
        .unwrap()
        .len(),
        1
    );
    let second = call(
        &router,
        "POST",
        base,
        writer,
        json!({"name":"Secret Readers"}),
        ("teams/create", 201),
    )
    .await;
    let secret = format!("{base}/secret-readers");
    assert_eq!(second["privacy"], "secret");
    call(
        &router,
        "GET",
        &secret,
        member,
        Value::Null,
        ("teams/get-by-name", 404),
    )
    .await;
    assert_eq!(
        call(
            &router,
            "GET",
            base,
            member,
            Value::Null,
            ("teams/list", 200)
        )
        .await
        .as_array()
        .unwrap()
        .len(),
        1
    );
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("{base}?per_page=1"))
                .header("authorization", reader)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["link"]
        .to_str()
        .unwrap()
        .contains("page=2"));
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_github_schema("teams/list", 200, &body);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(
        call(
            &router,
            "GET",
            &format!("{base}?page=2&per_page=1"),
            reader,
            Value::Null,
            ("teams/list", 200)
        )
        .await[0]["id"],
        second["id"]
    );
    assert_eq!(
        call(
            &router,
            "GET",
            "/api/v3/user/teams",
            member,
            Value::Null,
            ("teams/list-for-authenticated-user", 200)
        )
        .await[0]["id"],
        id
    );
    for (suffix, op, count) in [
        ("/members", "teams/list-members-in-org", 2),
        ("/members?role=maintainer", "teams/list-members-in-org", 2),
        (
            "/members?role=all&per_page=1",
            "teams/list-members-in-org",
            1,
        ),
    ] {
        let rows = call(
            &router,
            "GET",
            &format!("{path}{suffix}"),
            reader,
            Value::Null,
            (op, 200),
        )
        .await;
        assert_eq!(rows.as_array().unwrap().len(), count);
    }
    call(
        &router,
        "GET",
        &format!("{legacy}/members"),
        reader,
        Value::Null,
        ("teams/list-members-legacy", 200),
    )
    .await;
    call(
        &router,
        "GET",
        &format!("{legacy}/members/bob"),
        reader,
        Value::Null,
        ("teams/get-member-legacy", 204),
    )
    .await;
    for (base, suffix) in [(&path, "in-org"), (&legacy, "legacy")] {
        let membership = format!("{base}/memberships/bob");
        let get = format!("teams/get-membership-for-user-{suffix}");
        let put = format!("teams/add-or-update-membership-for-user-{suffix}");
        let delete = format!("teams/remove-membership-for-user-{suffix}");
        call(
            &router,
            "PUT",
            &membership,
            reader,
            json!({"role":"member"}),
            (&put, 403),
        )
        .await;
        call(
            &router,
            "PUT",
            &membership,
            writer,
            json!({"role":"owner"}),
            (&put, 422),
        )
        .await;
        assert_eq!(
            call(
                &router,
                "PUT",
                &membership,
                writer,
                json!({"role":"member"}),
                (&put, 200)
            )
            .await["role"],
            "member"
        );
        assert_eq!(
            call(
                &router,
                "GET",
                &membership,
                reader,
                Value::Null,
                (&get, 200)
            )
            .await["state"],
            "active"
        );
        call(
            &router,
            "DELETE",
            &membership,
            writer,
            Value::Null,
            (&delete, 204),
        )
        .await;
        call(
            &router,
            "GET",
            &membership,
            reader,
            Value::Null,
            (&get, 404),
        )
        .await;
        call(
            &router,
            "PUT",
            &membership,
            writer,
            json!({"role":"maintainer"}),
            (&put, 200),
        )
        .await;
    }
    call(
        &router,
        "DELETE",
        &format!("{legacy}/members/bob"),
        writer,
        Value::Null,
        ("teams/remove-member-legacy", 204),
    )
    .await;
    call(
        &router,
        "GET",
        &format!("{legacy}/members/bob"),
        reader,
        Value::Null,
        ("teams/get-member-legacy", 404),
    )
    .await;
    call(
        &router,
        "PUT",
        &format!("{legacy}/members/bob"),
        writer,
        json!({}),
        ("teams/add-member-legacy", 204),
    )
    .await;
    let repo_id = session
        .expect(
            "POST",
            "/api/repos",
            json!({"name":"team-repo","is_private":true,"owner_org_id":org.id}),
            StatusCode::CREATED,
        )
        .await["id"]
        .as_i64()
        .unwrap();
    for (base, suffix) in [(&path, "in-org"), (&legacy, "legacy")] {
        let repo_path = format!("{base}/repos/team-org/team-repo");
        let put = format!("teams/add-or-update-repo-permissions-{suffix}");
        let get = format!("teams/check-permissions-for-repo-{suffix}");
        let list = format!("teams/list-repos-{suffix}");
        let delete = format!("teams/remove-repo-{suffix}");
        call(
            &router,
            "PUT",
            &repo_path,
            reader,
            json!({"permission":"push"}),
            (&put, 403),
        )
        .await;
        call(
            &router,
            "PUT",
            &repo_path,
            writer,
            json!({"permission":"owner"}),
            (&put, 422),
        )
        .await;
        call(&router, "PUT", &repo_path, writer, json!({}), (&put, 204)).await;
        call(
            &router,
            "PUT",
            &repo_path,
            member,
            json!({"permission":"admin"}),
            (&put, 403),
        )
        .await;
        call(
            &router,
            "GET",
            &repo_path,
            other_member,
            Value::Null,
            (&get, 404),
        )
        .await;
        assert_eq!(
            call(
                &router,
                "GET",
                &format!("{base}/repos"),
                other_member,
                Value::Null,
                (&list, 200)
            )
            .await,
            json!([])
        );
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&repo_path)
                    .header("authorization", reader)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
        let repo = call(&router, "GET", &repo_path, reader, Value::Null, (&get, 200)).await;
        assert_eq!(repo["permissions"]["push"], true);
        assert_eq!(repo["permissions"]["admin"], false);
        assert_eq!(repo["id"], repo_id);
        call(
            &router,
            "GET",
            &repo_path,
            outsider,
            Value::Null,
            (&get, 404),
        )
        .await;
        assert_eq!(
            call(
                &router,
                "GET",
                &format!("{base}/repos"),
                reader,
                Value::Null,
                (&list, 200)
            )
            .await
            .as_array()
            .unwrap()
            .len(),
            1
        );
        call(
            &router,
            "DELETE",
            &repo_path,
            writer,
            Value::Null,
            (&delete, 204),
        )
        .await;
        call(&router, "GET", &repo_path, reader, Value::Null, (&get, 404)).await;
    }
    for (permission, level) in [
        ("pull", 1),
        ("triage", 2),
        ("push", 3),
        ("maintain", 4),
        ("admin", 5),
    ] {
        let repo_path = format!("{path}/repos/team-org/team-repo");
        call(
            &router,
            "PUT",
            &repo_path,
            writer,
            json!({"permission":permission}),
            ("teams/add-or-update-repo-permissions-in-org", 204),
        )
        .await;
        let repo = call(
            &router,
            "GET",
            &repo_path,
            reader,
            Value::Null,
            ("teams/check-permissions-for-repo-in-org", 200),
        )
        .await;
        for (field, minimum) in [
            ("pull", 1),
            ("triage", 2),
            ("push", 3),
            ("maintain", 4),
            ("admin", 5),
        ] {
            assert_eq!(repo["permissions"][field], level >= minimum);
        }
        let row = codeyard_db_repos::repositories::records::get_repository_by_id(f.pool(), repo_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            codeyard_db_orgs::organizations::access::get_organization_repo_access(
                f.pool(),
                bob,
                &row
            )
            .await
            .unwrap(),
            codeyard_db_core::models::AccessLevel::from(level)
        );
    }
    // A fine-grained token selected for another repository cannot change team access to
    // team-repo, through the team's repository route or the team's creation.
    let other_repo = session
        .expect(
            "POST",
            "/api/repos",
            json!({"name":"other-repo","is_private":true,"owner_org_id":org.id}),
            StatusCode::CREATED,
        )
        .await["id"]
        .as_i64()
        .unwrap();
    {
        let created = codeyard_identity::fine_grained::create(
            f.pool(),
            alice,
            Some(&session.session),
            codeyard_identity::fine_grained::Request {
                name: "team-admin".into(),
                description: "Team administration".into(),
                owner: codeyard_identity::fine_grained::Owner::Organization(org.id),
                access: codeyard_identity::fine_grained::Access::Selected,
                repositories: vec![other_repo],
                permissions: json!({"members":"write","administration":"write"})
                    .as_object()
                    .unwrap()
                    .clone(),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::days(30)),
            },
        )
        .await
        .unwrap();
        codeyard_db_identity::fine_grained_tokens::review(
            f.pool(),
            org.id,
            alice,
            &[created.id],
            "pending",
            "approved",
            None,
        )
        .await
        .unwrap();
        let fine = format!("Bearer {}", created.value);
        call(
            &router,
            "PUT",
            &format!("{path}/repos/team-org/team-repo"),
            &fine,
            json!({"permission":"admin"}),
            ("teams/add-or-update-repo-permissions-in-org", 404),
        )
        .await;
        call(
            &router,
            "POST",
            base,
            &fine,
            json!({"name":"Escalated","repo_names":["team-org/team-repo"]}),
            ("teams/create", 404),
        )
        .await;
        // Nor can it read that grant: the team's repository list and check keep to the
        // token's repositories.
        assert_eq!(
            call(
                &router,
                "GET",
                &format!("{path}/repos"),
                &fine,
                Value::Null,
                ("teams/list-repos-in-org", 200),
            )
            .await,
            json!([])
        );
        call(
            &router,
            "GET",
            &format!("{path}/repos/team-org/team-repo"),
            &fine,
            Value::Null,
            ("teams/check-permissions-for-repo-in-org", 404),
        )
        .await;
    }
    // Team creation is refused before repository names resolve, so members and outsiders cannot
    // tell a private repository from a missing one.
    for token in [member, outsider] {
        for name in ["team-org/team-repo", "team-org/missing"] {
            call(
                &router,
                "POST",
                base,
                token,
                json!({"name":"Probe","repo_names":[name]}),
                ("teams/create", 403),
            )
            .await;
        }
    }
    // Slugs drop accents as GitHub's do.
    let accented = call(
        &router,
        "POST",
        base,
        writer,
        json!({"name":"My TEam Näme"}),
        ("teams/create", 201),
    )
    .await;
    assert_eq!(accented["slug"], "my-team-name");
    // A team named "members" still answers membership documents on the organization route.
    let members_team = call(
        &router,
        "POST",
        base,
        writer,
        json!({"name":"members"}),
        ("teams/create", 201),
    )
    .await;
    let membership = call(
        &router,
        "GET",
        &format!("{base}/members/memberships/alice"),
        reader,
        Value::Null,
        ("teams/get-membership-for-user-in-org", 200),
    )
    .await;
    assert_eq!(membership["role"], "maintainer");
    assert_eq!(membership["state"], "active");
    for team in [&accented, &members_team] {
        call(
            &router,
            "DELETE",
            &format!("/api/v3/teams/{}", team["id"].as_i64().unwrap()),
            writer,
            Value::Null,
            ("teams/delete-legacy", 204),
        )
        .await;
    }
    let with_repo = call(
        &router,
        "POST",
        base,
        writer,
        json!({"name":"Repository Readers","repo_names":["team-org/team-repo"]}),
        ("teams/create", 201),
    )
    .await;
    assert_eq!(with_repo["repos_count"], 1);
    call(
        &router,
        "DELETE",
        &format!("/api/v3/teams/{}", with_repo["id"].as_i64().unwrap()),
        writer,
        Value::Null,
        ("teams/delete-legacy", 204),
    )
    .await;
    let pending = format!("{path}/memberships/eve");
    assert_eq!(
        call(
            &router,
            "PUT",
            &pending,
            writer,
            json!({"role":"maintainer"}),
            ("teams/add-or-update-membership-for-user-in-org", 200)
        )
        .await["state"],
        "pending"
    );
    assert_eq!(
        call(
            &router,
            "GET",
            &pending,
            reader,
            Value::Null,
            ("teams/get-membership-for-user-in-org", 200)
        )
        .await["state"],
        "pending"
    );
    call(
        &router,
        "GET",
        &format!("{legacy}/members/eve"),
        reader,
        Value::Null,
        ("teams/get-member-legacy", 404),
    )
    .await;
    let invitations = format!("{path}/invitations");
    let invited = call(
        &router,
        "GET",
        &invitations,
        reader,
        Value::Null,
        ("teams/list-pending-invitations-in-org", 200),
    )
    .await;
    assert_eq!(invited[0]["login"], "eve");
    assert_eq!(invited[0]["team_count"], 1);
    call(
        &router,
        "GET",
        &invitations,
        member,
        Value::Null,
        ("teams/list-pending-invitations-in-org", 403),
    )
    .await;
    call(
        &router,
        "GET",
        &format!("{legacy}/invitations"),
        reader,
        Value::Null,
        ("teams/list-pending-invitations-legacy", 200),
    )
    .await;
    call(
        &router,
        "DELETE",
        &pending,
        writer,
        Value::Null,
        ("teams/remove-membership-for-user-in-org", 204),
    )
    .await;
    assert_eq!(
        call(
            &router,
            "GET",
            &invitations,
            reader,
            Value::Null,
            ("teams/list-pending-invitations-in-org", 200)
        )
        .await,
        json!([])
    );
    call(
        &router,
        "PUT",
        &format!("{legacy}/memberships/eve"),
        writer,
        json!({"role":"maintainer"}),
        ("teams/add-or-update-membership-for-user-legacy", 200),
    )
    .await;
    let eve_session = Client::login(&router, f.pool(), eve).await;
    let reply = eve_session
        .form_reply(
            &format!("/settings/organizations/invitations/{}/accept", org.id),
            &[],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let accepted = call(
        &router,
        "GET",
        &pending,
        reader,
        Value::Null,
        ("teams/get-membership-for-user-in-org", 200),
    )
    .await;
    assert_eq!(accepted["state"], "active");
    assert_eq!(accepted["role"], "maintainer");
    assert_eq!(
        call(
            &router,
            "GET",
            &invitations,
            reader,
            Value::Null,
            ("teams/list-pending-invitations-in-org", 200)
        )
        .await,
        json!([])
    );
    let dave = f.actor("dave").await;
    let form_path = format!("/settings/organizations/{}?tab=teams&team={id}", org.id);
    let team_id = id.to_string();
    let reply = session
        .form_reply(
            &form_path,
            &[
                ("action", "team-member"),
                ("team_id", &team_id),
                ("username", "dave"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert_eq!(
        call(
            &router,
            "GET",
            &invitations,
            reader,
            Value::Null,
            ("teams/list-pending-invitations-in-org", 200)
        )
        .await[0]["login"],
        "dave"
    );
    let dave_session = Client::login(&router, f.pool(), dave).await;
    let reply = dave_session
        .form_reply(
            &format!("/settings/organizations/invitations/{}/decline", org.id),
            &[],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert_eq!(
        call(
            &router,
            "GET",
            &invitations,
            reader,
            Value::Null,
            ("teams/list-pending-invitations-in-org", 200)
        )
        .await,
        json!([])
    );
    nested::run(
        &router,
        &session,
        (f.pool(), org.id, alice, charlie, repo_id),
        (writer, reader, other_member),
    )
    .await;
    call(
        &router,
        "PATCH",
        &path,
        writer,
        json!({"name":"Renamed Writers"}),
        ("teams/update-in-org", 200),
    )
    .await;
    call(
        &router,
        "GET",
        &path,
        reader,
        Value::Null,
        ("teams/get-by-name", 404),
    )
    .await;
    call(
        &router,
        "DELETE",
        &format!("{base}/renamed-writers"),
        writer,
        Value::Null,
        ("teams/delete-in-org", 204),
    )
    .await;
    call(
        &router,
        "DELETE",
        &format!("/api/v3/teams/{}", second["id"].as_i64().unwrap()),
        writer,
        Value::Null,
        ("teams/delete-legacy", 204),
    )
    .await;
    assert_eq!(
        call(
            &router,
            "GET",
            base,
            reader,
            Value::Null,
            ("teams/list", 200)
        )
        .await,
        json!([])
    );
    // Deleting a team never deletes the organization's repository or membership.
    assert!(
        codeyard_db_repos::repositories::records::get_repository_by_id(f.pool(), repo_id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn sqlite_github_teams() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_teams() {
    contract(true).await;
}

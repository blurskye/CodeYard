use super::{db, json, protection::call, request, Client, Router, Value};
use crate::support::organizations::applied;
use axum::http::StatusCode;
use codeyard::backend::db::{
    models::{AccessLevel, OrgRole},
    organizations as orgs,
};

pub(super) async fn run(
    router: &Router,
    scope: (&db::DbPool, i64, i64),
    tokens: (&str, &str, &str),
    session: &Client,
) {
    let (pool, alice, bob) = scope;
    let (writer, reader, bob_token) = tokens;
    let org = applied(
        orgs::create_organization(
            pool,
            alice,
            &orgs::NewOrganization::new(
                "push-org",
                orgs::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    applied(
        orgs::add_organization_member(pool, alice, org, bob, OrgRole::Member)
            .await
            .unwrap(),
    );
    let writers = applied(
        orgs::create_organization_team(
            pool,
            alice,
            org,
            &orgs::NewTeam::new(
                "writers",
                orgs::TeamProfile::new(
                    "Writers",
                    Some("Release publishers"),
                    orgs::TeamPrivacy::Secret,
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    let readers = applied(
        orgs::create_organization_team(
            pool,
            alice,
            org,
            &orgs::NewTeam::new(
                "readers",
                orgs::TeamProfile::new("Readers", None, orgs::TeamPrivacy::Visible).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    applied(
        orgs::add_organization_team_member(pool, alice, org, writers, bob, orgs::TeamRole::Member)
            .await
            .unwrap(),
    );
    let repo = session
        .expect(
            "POST",
            "/api/repos",
            json!({"name":"team-policy","is_private":true,"owner_org_id":org}),
            StatusCode::CREATED,
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let tip = session.expect("POST", "/api/repos/push-org/team-policy/contents/seed.txt", json!({"content":"c2VlZAo=","message":"Seed team policy","branch":"main","expected_revision":"unborn"}), StatusCode::CREATED).await["sha"].as_str().unwrap().to_owned();
    for (team, permission) in [(writers, 3), (readers, 1)] {
        applied(
            orgs::grant_organization_team_repository(
                pool,
                alice,
                org,
                team,
                repo,
                orgs::TeamRepositoryPermission::new(permission).unwrap(),
            )
            .await
            .unwrap(),
        );
    }
    db::repositories::access::grant_repo_access(pool, bob, repo, AccessLevel::Write)
        .await
        .unwrap();
    db::repositories::protection::upsert_branch_protection_rule(
        pool,
        db::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: alice,
            repo_id: repo,
            pattern: "main",
            require_pull_request: false,
            restrict_pushes: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let base = "/api/v3/repos/push-org/team-policy";
    let path = format!("{base}/branches/main/protection/restrictions/teams");
    assert_eq!(
        call(
            router,
            "GET",
            &path,
            reader,
            Value::Null,
            ("repos/get-teams-with-access-to-protected-branch", 200)
        )
        .await,
        json!([])
    );
    call(
        router,
        "PUT",
        &path,
        reader,
        json!(["writers"]),
        ("repos/set-team-access-restrictions", 403),
    )
    .await;
    call(
        router,
        "GET",
        &path,
        bob_token,
        Value::Null,
        ("repos/get-teams-with-access-to-protected-branch", 404),
    )
    .await;
    for bad in [
        json!(["missing"]),
        json!(["readers"]),
        json!([""]),
        json!(["bad\0name"]),
        json!(["\nwriters"]),
        json!(["writers\t"]),
        json!(["x".repeat(256)]),
        json!(vec!["writers"; 101]),
    ] {
        call(
            router,
            "PUT",
            &path,
            writer,
            bad,
            ("repos/set-team-access-restrictions", 422),
        )
        .await;
    }
    let allowed = call(
        router,
        "PUT",
        &path,
        writer,
        json!({"teams":["WRITERS","writers"]}),
        ("repos/set-team-access-restrictions", 200),
    )
    .await;
    assert_eq!(allowed.as_array().unwrap().len(), 1);
    assert_eq!(allowed[0]["id"], writers);
    assert_eq!(allowed[0]["privacy"], "secret");
    assert_eq!(allowed[0]["permission"], "push");
    assert_eq!(allowed[0]["type"], "organization");
    assert_eq!(allowed[0]["parent"], Value::Null);
    let restrictions = format!("{base}/branches/main/protection/restrictions");
    assert_eq!(
        call(
            router,
            "GET",
            &restrictions,
            reader,
            Value::Null,
            ("repos/get-access-restrictions", 200)
        )
        .await["teams"],
        allowed
    );
    for token in [reader, writer] {
        let (status, _, body) = request(router, "HEAD", &path, Some(token), Value::Null).await;
        assert_eq!(status.as_u16(), 200);
        assert_eq!(body, Value::Null);
    }
    assert_eq!(
        call(
            router,
            "GET",
            &path,
            reader,
            Value::Null,
            ("repos/get-teams-with-access-to-protected-branch", 200)
        )
        .await,
        allowed
    );
    assert_eq!(
        call(
            router,
            "POST",
            &path,
            writer,
            json!(["writers"]),
            ("repos/add-team-access-restrictions", 200)
        )
        .await,
        allowed
    );
    let original = call(
        router,
        "GET",
        &format!("{base}/git/commits/{tip}"),
        reader,
        Value::Null,
        ("git/get-commit", 200),
    )
    .await;
    let candidate = call(router, "POST", &format!("{base}/git/commits"), writer, json!({"message":"Published by an allowed team member","tree":original["tree"]["sha"],"parents":[tip]}), ("git/create-commit", 201)).await;
    let reference = format!("{base}/git/refs/heads/main");
    call(
        router,
        "PATCH",
        &reference,
        writer,
        json!({"sha":candidate["sha"]}),
        ("git/update-ref", 403),
    )
    .await;
    call(
        router,
        "PATCH",
        &reference,
        bob_token,
        json!({"sha":candidate["sha"]}),
        ("git/update-ref", 200),
    )
    .await;
    applied(
        orgs::remove_organization_team_member(pool, alice, org, writers, bob)
            .await
            .unwrap(),
    );
    call(
        router,
        "PATCH",
        &reference,
        bob_token,
        json!({"sha":candidate["sha"]}),
        ("git/update-ref", 403),
    )
    .await;
    applied(
        orgs::add_organization_team_member(pool, alice, org, writers, bob, orgs::TeamRole::Member)
            .await
            .unwrap(),
    );
    applied(
        orgs::revoke_organization_team_repository(pool, alice, org, writers, repo)
            .await
            .unwrap(),
    );
    call(
        router,
        "PATCH",
        &reference,
        bob_token,
        json!({"sha":candidate["sha"]}),
        ("git/update-ref", 403),
    )
    .await;
    assert_eq!(
        call(
            router,
            "GET",
            &path,
            reader,
            Value::Null,
            ("repos/get-teams-with-access-to-protected-branch", 200)
        )
        .await[0]["permission"],
        "none"
    );
    assert_eq!(
        call(
            router,
            "DELETE",
            &path,
            writer,
            json!(["writers"]),
            ("repos/remove-team-access-restrictions", 200)
        )
        .await,
        json!([]),
        "revoked allowances remain removable"
    );
    applied(
        orgs::grant_organization_team_repository(
            pool,
            alice,
            org,
            writers,
            repo,
            orgs::TeamRepositoryPermission::new(3).unwrap(),
        )
        .await
        .unwrap(),
    );

    let native = "/push-org/team-policy";
    let html = session
        .page_ok(&format!("{native}/settings?tab=branches"))
        .await;
    assert!(html.contains("Teams allowed to push"));
    let repo_id = repo.to_string();
    let form = format!("{native}/_forms/protection-save");
    let saved = session
        .form_reply(
            &form,
            &[
                ("repository_id", &repo_id),
                ("pattern", "main"),
                ("required_approving_reviews", "0"),
                ("restrict_pushes", "on"),
                ("push_team_allowances", "writers"),
                ("enforce_admins", "on"),
                ("block_force_pushes", "on"),
                ("block_deletions", "on"),
            ],
        )
        .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.text());
    assert_eq!(
        call(
            router,
            "GET",
            &path,
            reader,
            Value::Null,
            ("repos/get-teams-with-access-to-protected-branch", 200)
        )
        .await,
        allowed
    );
    let (model_status, page) = session
        .model(&format!("{native}/settings?tab=branches"))
        .await;
    assert_eq!(model_status, StatusCode::OK);
    let codeyard::webui::app::Page::Repositories(
        codeyard::webui::pages::repos::page::Page::Settings { settings, .. },
    ) = page
    else {
        panic!("repository settings model")
    };
    assert_eq!(
        settings
            .branch_rules
            .iter()
            .find(|rule| rule.pattern == "main")
            .unwrap()
            .push_team_allowances,
        vec!["writers"]
    );
    let (status, html) = session
        .form(
            &form,
            &[
                ("repository_id", &repo_id),
                ("pattern", "main"),
                ("required_approving_reviews", "0"),
                ("restrict_pushes", "on"),
                ("push_team_allowances", "missing-draft-team"),
                ("enforce_admins", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{html}");
    assert!(html.contains("missing-draft-team"));
    assert_eq!(
        call(
            router,
            "GET",
            &path,
            reader,
            Value::Null,
            ("repos/get-teams-with-access-to-protected-branch", 200)
        )
        .await,
        allowed
    );
    call(
        router,
        "DELETE",
        &restrictions,
        writer,
        Value::Null,
        ("repos/delete-access-restrictions", 204),
    )
    .await;
    call(
        router,
        "PATCH",
        &reference,
        writer,
        json!({"sha":candidate["sha"]}),
        ("git/update-ref", 200),
    )
    .await;
    call(
        router,
        "GET",
        &path,
        reader,
        Value::Null,
        ("repos/get-teams-with-access-to-protected-branch", 404),
    )
    .await;
}

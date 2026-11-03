use super::{
    applied, organizations, request, tokens, Client, Database, NewOrganization, OrgRole,
    OrganizationProfile, TokenInput,
};
use axum::http::StatusCode;
use codeyard::backend::db::repositories::creation::{RepositoryCreationOwner, RepositoryTarget};
use serde_json::{json, Value};

const BASE: &str = "/api/v3/orgs/acme/dependabot/repository-access";

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let outsider = f.actor("outsider").await;
    let mut orgs = Vec::new();
    for name in ["acme", "second"] {
        orgs.push(applied(
            organizations::create_organization(
                f.pool(),
                owner,
                &NewOrganization::new(name, OrganizationProfile::new(None, None).unwrap()).unwrap(),
            )
            .await
            .unwrap(),
        ));
    }
    let org = orgs[0].id;
    applied(
        organizations::add_organization_member(f.pool(), owner, org, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let mut repositories = Vec::new();
    for (org, name, private) in [
        (org, "one", true),
        (org, "two", true),
        (org, "public", false),
        (orgs[1].id, "foreign", true),
    ] {
        repositories.push(
            codeyard::backend::repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                RepositoryCreationOwner {
                    actor_id: owner,
                    target: RepositoryTarget::Organization(org),
                },
                name,
                None,
                private,
                None,
            )
            .await
            .unwrap(),
        );
    }
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), owner).await;
    let member_session = Client::login(&router, f.pool(), member).await;
    let other_session = Client::login(&router, f.pool(), outsider).await;
    let mut credentials = Vec::new();
    for (actor, session, scope) in [
        (owner, &session, "write"),
        (owner, &session, "read"),
        (member, &member_session, "write"),
        (outsider, &other_session, "write"),
    ] {
        let token = tokens::create(
            f.pool(),
            actor,
            Some(&session.session),
            TokenInput {
                name: "dependabot access",
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let writer = Some(credentials[0].as_str());
    let reader = Some(credentials[1].as_str());
    let (_, access) = request(
        &router,
        "dependabot/repository-access-for-org",
        "GET",
        BASE,
        reader,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(access["default_level"], "public");
    assert_eq!(
        access["accessible_repositories"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        access["accessible_repositories"][0]["id"],
        repositories[2].id
    );
    request(
        &router,
        "dependabot/update-repository-access-for-org",
        "PATCH",
        BASE,
        writer,
        json!({"repository_ids_to_add":[repositories[0].id,repositories[1].id,repositories[0].id]}),
        204,
    )
    .await;
    let (headers, access) = request(
        &router,
        "dependabot/repository-access-for-org",
        "GET",
        &format!("{BASE}?per_page=1"),
        reader,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(
        access["accessible_repositories"].as_array().unwrap().len(),
        1
    );
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (_, access) = request(
        &router,
        "dependabot/repository-access-for-org",
        "GET",
        &format!("{BASE}?per_page=1&page=4"),
        reader,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(access["accessible_repositories"], json!([]));
    for input in [
        json!({"repository_ids_to_add":[repositories[3].id], "repository_ids_to_remove":[repositories[0].id]}),
        json!({"repository_ids_to_add":[repositories[0].id], "repository_ids_to_remove":[repositories[0].id]}),
        json!({"repository_ids_to_add":[0]}),
    ] {
        request(
            &router,
            "dependabot/update-repository-access-for-org",
            "PATCH",
            BASE,
            writer,
            input,
            422,
        )
        .await;
    }
    let (_, access) = request(
        &router,
        "dependabot/repository-access-for-org",
        "GET",
        BASE,
        reader,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(
        access["accessible_repositories"].as_array().unwrap().len(),
        3
    );
    request(
        &router,
        "dependabot/set-repository-access-default-level",
        "PUT",
        &format!("{BASE}/default-level"),
        writer,
        json!({"default_level":"internal"}),
        204,
    )
    .await;
    let (_, access) = request(
        &router,
        "dependabot/repository-access-for-org",
        "GET",
        BASE,
        reader,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(access["default_level"], "internal");
    request(
        &router,
        "dependabot/set-repository-access-default-level",
        "PUT",
        &format!("{BASE}/default-level"),
        writer,
        json!({"default_level":"all"}),
        422,
    )
    .await;
    for (token, status) in [
        (None, 401),
        (Some(credentials[2].as_str()), 403),
        (Some(credentials[3].as_str()), 403),
    ] {
        request(
            &router,
            "dependabot/repository-access-for-org",
            "GET",
            BASE,
            token,
            Value::Null,
            status,
        )
        .await;
        request(
            &router,
            "dependabot/update-repository-access-for-org",
            "PATCH",
            BASE,
            token,
            json!({"repository_ids_to_remove":[repositories[0].id]}),
            status,
        )
        .await;
        request(
            &router,
            "dependabot/set-repository-access-default-level",
            "PUT",
            &format!("{BASE}/default-level"),
            token,
            json!({"default_level":"public"}),
            status,
        )
        .await;
    }
    request(
        &router,
        "dependabot/update-repository-access-for-org",
        "PATCH",
        BASE,
        reader,
        json!({}),
        403,
    )
    .await;
    request(
        &router,
        "dependabot/set-repository-access-default-level",
        "PUT",
        &format!("{BASE}/default-level"),
        reader,
        json!({"default_level":"public"}),
        403,
    )
    .await;

    let settings = format!("/settings/organizations/{org}?tab=dependabot");
    let target = format!("/settings/organizations/{org}/dependabot?tab=dependabot");
    let html = session.page_ok(&settings).await;
    assert!(html.contains("Dependabot repository access"), "{html}");
    assert!(html.contains(&target), "{html}");
    let (status, model) = session.model(&settings).await;
    assert_eq!(status, StatusCode::OK);
    let codeyard_webui::app::Page::Organizations(model) = model else {
        panic!("organization model");
    };
    assert_eq!(model.dependabot.unwrap().repositories.len(), 3);
    let reply = session
        .form_reply(
            &target,
            &[
                ("action", "dependabot-remove"),
                ("repo_id", &repositories[0].id.to_string()),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let (_, access) = request(
        &router,
        "dependabot/repository-access-for-org",
        "GET",
        BASE,
        reader,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(
        access["accessible_repositories"].as_array().unwrap().len(),
        2
    );
    let reply = session
        .form_reply(
            &target,
            &[
                ("action", "dependabot-add"),
                ("repo_id", &repositories[0].id.to_string()),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let reply = session
        .form_reply(
            &target,
            &[
                ("action", "dependabot-default"),
                ("default_level", "public"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let reply = session
        .form_reply(
            &target,
            &[("action", "dependabot-add"), ("repo_id", "bad-id")],
        )
        .await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.text()
    );
    assert!(reply.text().contains("bad-id"));
    let reply = member_session
        .form_reply(
            &target,
            &[
                ("action", "dependabot-default"),
                ("default_level", "public"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text());
    let reply = session
        .exchange(
            "POST",
            &target,
            "application/x-www-form-urlencoded",
            b"action=dependabot-default&default_level=public".to_vec(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    // Old grants must not disclose repositories transferred to another organization.
    f.execute(
        "UPDATE repositories SET owner_org_id=$2 WHERE id=$1",
        &[repositories[0].id, orgs[1].id],
    )
    .await;
    let (_, access) = request(
        &router,
        "dependabot/repository-access-for-org",
        "GET",
        BASE,
        reader,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(
        access["accessible_repositories"].as_array().unwrap().len(),
        2
    );
    request(
        &router,
        "dependabot/update-repository-access-for-org",
        "PATCH",
        BASE,
        writer,
        json!({"repository_ids_to_add":[repositories[0].id]}),
        422,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn sqlite_dependabot_repository_access() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_dependabot_repository_access() {
    contract(true).await;
}

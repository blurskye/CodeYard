use super::action_secrets::{pat, request, sealed};
use crate::support::database::Database;
use crate::support::fake_webui_bundle;
use crate::support::http_client::Client;
use crate::support::organizations::applied;
use axum::http::StatusCode;
use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;
use codeyard_support::secrets::SecretEncryptor;

use codeyard_db_orgs::organizations::profiles::NewOrganization;
use codeyard_db_orgs::organizations::profiles::OrganizationProfile;
use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
use codeyard_db_repos::repositories::creation::RepositoryTarget;

use codeyard_database::models::OrgRole;
use codeyard_storage::ObjectStore;
use serde_json::{json, Value};
use std::sync::Arc;

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("alice").await;
    let outsider = f.actor("bob").await;
    let site = f.actor("siteadmin").await;
    let manager = f.actor("manager").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[site])
        .await;
    let org = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("native_org", OrganizationProfile::new(None, None).unwrap())
                .unwrap(),
        )
        .await
        .unwrap(),
    );
    let other = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("other_org", OrganizationProfile::new(None, None).unwrap())
                .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        codeyard_db_orgs::organizations::membership::add_organization_member(
            f.pool(),
            owner,
            org.id,
            manager,
            OrgRole::Admin,
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (name, target, private) in [
        ("private", org.id, true),
        ("public", org.id, false),
        ("foreign", other.id, true),
    ] {
        repos.push(
            codeyard_git_storage::repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                RepositoryCreationOwner {
                    actor_id: owner,
                    target: RepositoryTarget::Organization(target),
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
    let encryptor = Arc::new(SecretEncryptor::new(&[0x63; 32]));
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(encryptor.clone()),
        CiConfig::default(),
        None,
    ));
    let writer = pat(&f, &router, owner, "write").await;
    let reader = pat(&f, &router, owner, "read").await;
    let outsider_token = pat(&f, &router, outsider, "write").await;
    let site_token = pat(&f, &router, site, "write").await;
    let manager_token = pat(&f, &router, manager, "write").await;
    let (_, key) = request(
        &router,
        "actions/get-org-public-key",
        "GET",
        "/api/v3/orgs/native_org/actions/secrets/public-key",
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    let (_, stable) = request(
        &router,
        "actions/get-org-public-key",
        "GET",
        "/api/v3/orgs/native_org/actions/secrets/public-key",
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(key, stable);
    let (_, other_key) = request(
        &router,
        "actions/get-org-public-key",
        "GET",
        "/api/v3/orgs/other_org/actions/secrets/public-key",
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_ne!(key["key_id"], other_key["key_id"]);
    for secret in [false, true] {
        let noun = if secret { "secret" } else { "variable" };
        let plural = if secret { "secrets" } else { "variables" };
        let base = format!("/api/v3/orgs/native_org/actions/{plural}");
        let create = if secret {
            "actions/create-or-update-org-secret"
        } else {
            "actions/create-org-variable"
        };
        let method = if secret { "PUT" } else { "POST" };
        let path = if secret {
            format!("{base}/region")
        } else {
            base.clone()
        };
        let mut input = if secret {
            sealed(&key, b"organization-private-marker")
        } else {
            json!({"name":"region","value":"org-region"})
        };
        input["visibility"] = json!("selected");
        input["selected_repository_ids"] = json!([repos[0].id]);
        request(
            &router,
            create,
            method,
            &path,
            Some(&writer),
            input.clone(),
            201,
        )
        .await;
        let (_, metadata) = request(
            &router,
            &format!("actions/get-org-{noun}"),
            "GET",
            &format!("{base}/region"),
            Some(&reader),
            Value::Null,
            200,
        )
        .await;
        assert_eq!(metadata["name"], "REGION");
        assert_eq!(metadata["visibility"], "selected");
        if secret {
            assert!(metadata.get("value").is_none());
            assert!(!metadata.to_string().contains("organization-private-marker"));
            for bad in [
                json!({"key_id":other_key["key_id"],"encrypted_value":input["encrypted_value"],"visibility":"private"}),
                json!({"key_id":key["key_id"],"encrypted_value":"bad","visibility":"private"}),
            ] {
                request(
                    &router,
                    create,
                    "PUT",
                    &format!("{base}/REGION"),
                    Some(&writer),
                    bad,
                    422,
                )
                .await;
            }
            let stored = codeyard_db_orgs::organization_values::workflow_values(
                f.pool(),
                repos[0].id,
                codeyard_db_orgs::organization_values::Kind::Secret,
            )
            .await
            .unwrap();
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].value, "");
            assert!(!stored[0]
                .encrypted_value
                .as_ref()
                .unwrap()
                .windows(b"organization-private-marker".len())
                .any(|part| part == b"organization-private-marker"));
            assert_eq!(
                codeyard_db_orgs::organization_values::secrets_for_run(
                    f.pool(),
                    &encryptor,
                    repos[0].id
                )
                .await
                .unwrap(),
                vec![(
                    "REGION".to_owned(),
                    "organization-private-marker".to_owned()
                )]
            );
        } else {
            assert_eq!(metadata["value"], "org-region");
            request(
                &router,
                create,
                method,
                &path,
                Some(&writer),
                input.clone(),
                409,
            )
            .await;
            super::action_variables::gh_empty_value(
                &router,
                &base,
                &base,
                &reader,
                &writer,
                Some("private"),
            )
            .await;
        }
        let selected = format!("{base}/REGION/repositories");
        let list_selected = format!("actions/list-selected-repos-for-org-{noun}");
        let set_selected = format!("actions/set-selected-repos-for-org-{noun}");
        let add_selected = format!("actions/add-selected-repo-to-org-{noun}");
        let remove_selected = format!("actions/remove-selected-repo-from-org-{noun}");
        let (_, selection) = request(
            &router,
            &list_selected,
            "GET",
            &selected,
            Some(&reader),
            Value::Null,
            200,
        )
        .await;
        assert_eq!(selection["repositories"][0]["id"], repos[0].id);
        request(
            &router,
            &set_selected,
            "PUT",
            &selected,
            Some(&writer),
            json!({"selected_repository_ids":[repos[1].id]}),
            204,
        )
        .await;
        request(
            &router,
            &add_selected,
            "PUT",
            &format!("{selected}/{}", repos[0].id),
            Some(&writer),
            Value::Null,
            204,
        )
        .await;
        let (headers, selection) = request(
            &router,
            &list_selected,
            "GET",
            &format!("{selected}?per_page=1"),
            Some(&reader),
            Value::Null,
            200,
        )
        .await;
        assert_eq!(selection["total_count"], 2);
        assert!(headers["link"].to_str().unwrap().contains("page=2"));
        request(
            &router,
            &remove_selected,
            "DELETE",
            &format!("{selected}/{}", repos[1].id),
            Some(&writer),
            Value::Null,
            204,
        )
        .await;
        request(
            &router,
            &set_selected,
            "PUT",
            &selected,
            Some(&writer),
            json!({"selected_repository_ids":[repos[1].id,repos[2].id]}),
            422,
        )
        .await;
        let (_, selection) = request(
            &router,
            &list_selected,
            "GET",
            &selected,
            Some(&reader),
            Value::Null,
            200,
        )
        .await;
        assert_eq!(
            selection["total_count"], 1,
            "failed replacement must be atomic"
        );
        assert_eq!(selection["repositories"][0]["id"], repos[0].id);
        for (operation, method, path, body) in [
            (
                create.to_owned(),
                if secret { "PUT" } else { "POST" },
                path.clone(),
                input.clone(),
            ),
            (
                set_selected.clone(),
                "PUT",
                selected.clone(),
                json!({"selected_repository_ids":[]}),
            ),
            (
                add_selected.clone(),
                "PUT",
                format!("{selected}/{}", repos[1].id),
                Value::Null,
            ),
            (
                remove_selected.clone(),
                "DELETE",
                format!("{selected}/{}", repos[0].id),
                Value::Null,
            ),
            (
                format!("actions/delete-org-{noun}"),
                "DELETE",
                format!("{base}/REGION"),
                Value::Null,
            ),
        ] {
            request(&router, &operation, method, &path, Some(&reader), body, 403).await;
        }
        for token in [&outsider_token, &site_token] {
            request(
                &router,
                &format!("actions/list-org-{plural}"),
                "GET",
                &base,
                Some(token),
                Value::Null,
                403,
            )
            .await;
        }
        request(
            &router,
            &format!("actions/list-org-{plural}"),
            "GET",
            &base,
            None,
            Value::Null,
            401,
        )
        .await;
        let (_, list) = request(
            &router,
            &format!("actions/list-org-{plural}"),
            "GET",
            &format!("{base}?per_page=1"),
            Some(&manager_token),
            Value::Null,
            200,
        )
        .await;
        assert_eq!(list["total_count"], 1);
        assert_eq!(list[plural][0]["name"], metadata["name"]);
        assert_eq!(list[plural][0]["created_at"], metadata["created_at"]);
        let gh_list = super::action_secrets::gh_list(
            &router,
            &format!("actions/list-org-{plural}"),
            &base,
            &reader,
            plural,
        )
        .await;
        assert_eq!(gh_list, list);
        for (repo, count) in [("private", 1), ("public", 0)] {
            let (_, inherited) = request(
                &router,
                &format!("actions/list-repo-organization-{plural}"),
                "GET",
                &format!("/api/v3/repos/native_org/{repo}/actions/organization-{plural}"),
                Some(&reader),
                Value::Null,
                200,
            )
            .await;
            assert_eq!(inherited["total_count"], count);
        }
        request(
            &router,
            &format!("actions/list-repo-organization-{plural}"),
            "GET",
            &format!("/api/v3/repos/native_org/private/actions/organization-{plural}"),
            Some(&outsider_token),
            Value::Null,
            404,
        )
        .await;
        let final_name = if secret { "REGION" } else { "RENAMED" };
        if secret {
            input["visibility"] = json!("private");
            input
                .as_object_mut()
                .unwrap()
                .remove("selected_repository_ids");
            request(
                &router,
                create,
                "PUT",
                &path,
                Some(&writer),
                input.clone(),
                204,
            )
            .await;
        } else {
            request(
                &router,
                "actions/update-org-variable",
                "PATCH",
                &format!("{base}/REGION"),
                Some(&reader),
                json!({"value":"bad"}),
                403,
            )
            .await;
            request(
                &router,
                "actions/update-org-variable",
                "PATCH",
                &format!("{base}/REGION"),
                Some(&writer),
                json!({"name":"renamed"}),
                204,
            )
            .await;
            let (_, selection) = request(
                &router,
                &list_selected,
                "GET",
                &format!("{base}/RENAMED/repositories"),
                Some(&reader),
                Value::Null,
                200,
            )
            .await;
            assert_eq!(
                selection["repositories"][0]["id"], repos[0].id,
                "rename preserves selection"
            );
            request(
                &router,
                "actions/update-org-variable",
                "PATCH",
                &format!("{base}/RENAMED"),
                Some(&writer),
                json!({"visibility":"private","value":""}),
                204,
            )
            .await;
        }
        let (_, updated) = request(
            &router,
            &format!("actions/get-org-{noun}"),
            "GET",
            &format!("{base}/{final_name}"),
            Some(&reader),
            Value::Null,
            200,
        )
        .await;
        assert_eq!(updated["created_at"], metadata["created_at"]);
        assert_eq!(updated["visibility"], "private");
        request(
            &router,
            &add_selected,
            "PUT",
            &format!("{base}/{final_name}/repositories/{}", repos[0].id),
            Some(&writer),
            Value::Null,
            409,
        )
        .await;
        request(
            &router,
            &format!("actions/delete-org-{noun}"),
            "DELETE",
            &format!("{base}/{final_name}"),
            Some(&writer),
            Value::Null,
            204,
        )
        .await;
        request(
            &router,
            &format!("actions/get-org-{noun}"),
            "GET",
            &format!("{base}/{final_name}"),
            Some(&reader),
            Value::Null,
            404,
        )
        .await;
    }
    for body in [
        json!({"name":"GITHUB_TOKEN","value":"bad","visibility":"all"}),
        json!({"name":"BAD-NAME","value":"bad","visibility":"all"}),
        json!({"name":"NUL","value":"bad\0value","visibility":"all"}),
        json!({"name":"LARGE","value":"x".repeat(49153),"visibility":"all"}),
        json!({"name":"WRONG_VISIBILITY","value":"bad","visibility":"public"}),
        json!({"name":"WRONG_SELECTION","value":"bad","visibility":"all","selected_repository_ids":[repos[0].id]}),
    ] {
        let (_, failure) = request(
            &router,
            "actions/create-org-variable",
            "POST",
            "/api/v3/orgs/native_org/actions/variables",
            Some(&writer),
            body,
            422,
        )
        .await;
        assert!(!failure["errors"].as_array().unwrap().is_empty());
    }
    let client = Client::login(&router, f.pool(), owner).await;
    let base = format!("/settings/organizations/{}/actions", org.id);
    let id = org.id.to_string();
    let reply = client
        .form_reply(
            &format!("{base}/secret-save"),
            &[
                ("org_id", &id),
                ("name", "UI_SECRET"),
                ("value", "native-form-private-marker"),
                ("visibility", "all"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let page = client
        .page_ok(&format!("/settings/organizations/{}?tab=actions", org.id))
        .await;
    assert!(page.contains("UI_SECRET"));
    assert!(!page.contains("native-form-private-marker"));
    let reply = client
        .form_reply(
            &format!("{base}/secret-access"),
            &[
                ("org_id", &id),
                ("name", "UI_SECRET"),
                ("visibility", "selected"),
                ("repositories", &repos[0].id.to_string()),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert_eq!(
        codeyard_db_orgs::organization_values::secrets_for_run(f.pool(), &encryptor, repos[0].id)
            .await
            .unwrap()[0]
            .1,
        "native-form-private-marker"
    );
    assert!(codeyard_db_orgs::organization_values::secrets_for_run(
        f.pool(),
        &encryptor,
        repos[1].id
    )
    .await
    .unwrap()
    .is_empty());
    let reply = client
        .form_reply(
            &format!("{base}/secret-save"),
            &[
                ("org_id", &id),
                ("name", "GITHUB_BAD"),
                ("value", "do-not-echo-refused-secret"),
                ("visibility", "all"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!reply.text().contains("do-not-echo-refused-secret"));
    let reply = client
        .form_reply(
            &format!("{base}/variable-create"),
            &[
                ("org_id", &id),
                ("name", "UI_VARIABLE"),
                ("value", "readable variable"),
                ("visibility", "private"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let reply = client
        .form_reply(
            &format!("{base}/secret-delete"),
            &[("org_id", &id), ("name", "UI_SECRET"), ("confirm", "yes")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    applied(
        codeyard_db_orgs::organizations::membership::update_organization_member_role(
            f.pool(),
            owner,
            org.id,
            manager,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    request(
        &router,
        "actions/list-org-variables",
        "GET",
        "/api/v3/orgs/native_org/actions/variables",
        Some(&manager_token),
        Value::Null,
        403,
    )
    .await;
    assert!(matches!(
        codeyard_db_orgs::organization_values::save(
            f.pool(),
            None,
            manager,
            &codeyard_database::queries::users::accounts::AccountCredential::Session(
                &crate::support::live_session(f.pool(), manager).await
            ),
            org.id,
            &codeyard_db_orgs::organization_values::Save {
                kind: codeyard_db_orgs::organization_values::Kind::Variable,
                previous: None,
                name: "DENIED",
                value: Some("bad"),
                visibility: Some("all"),
                repositories: None
            }
        )
        .await,
        Err(codeyard_db_orgs::organization_values::ValueError::Denied)
    ));
    f.finish().await;
}

#[tokio::test]
async fn sqlite_organization_values_contract() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_organization_values_contract() {
    contract(true).await;
}

async fn transfer_selections(pg: bool) {
    use codeyard_db_orgs::organization_values as values;
    use codeyard_db_orgs::organization_values::Kind;
    use codeyard_db_orgs::organization_values::Save;
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("transfer_owner").await;
    let mut organizations = Vec::new();
    for name in ["source", "target"] {
        organizations.push(applied(
            codeyard_db_orgs::organizations::profiles::create_organization(
                f.pool(),
                owner,
                &NewOrganization::new(name, OrganizationProfile::new(None, None).unwrap()).unwrap(),
            )
            .await
            .unwrap(),
        ));
    }
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_git_storage::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner {
            actor_id: owner,
            target: RepositoryTarget::Organization(organizations[0].id),
        },
        "selected",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let encryptor = SecretEncryptor::new(&[0x63; 32]);
    for kind in [Kind::Secret, Kind::Variable] {
        values::save(
            f.pool(),
            Some(&encryptor),
            owner,
            &codeyard_database::queries::users::accounts::AccountCredential::Session(
                &crate::support::live_session(f.pool(), owner).await,
            ),
            organizations[0].id,
            &Save {
                kind,
                previous: None,
                name: "SELECTED",
                value: Some("scoped-value"),
                visibility: Some("selected"),
                repositories: Some(&[repo.id]),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            values::repositories(
                f.pool(),
                owner,
                organizations[0].id,
                kind,
                "SELECTED",
                0,
                100
            )
            .await
            .unwrap()
            .1
            .len(),
            1
        );
    }
    codeyard_git_storage::repositories::lifecycle::transfer(
        f.pool(),
        None,
        owner,
        repo.id,
        &organizations[1].name,
    )
    .await
    .unwrap();
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM org_actions_value_repositories WHERE repo_id=$1",
            &[repo.id]
        )
        .await,
        0
    );
    for kind in [Kind::Secret, Kind::Variable] {
        assert!(values::workflow_values(f.pool(), repo.id, kind)
            .await
            .unwrap()
            .is_empty());
        assert!(values::repositories(
            f.pool(),
            owner,
            organizations[0].id,
            kind,
            "SELECTED",
            0,
            100
        )
        .await
        .unwrap()
        .1
        .is_empty());
    }
    codeyard_git_storage::repositories::lifecycle::transfer(
        f.pool(),
        None,
        owner,
        repo.id,
        &organizations[0].name,
    )
    .await
    .unwrap();
    for kind in [Kind::Secret, Kind::Variable] {
        assert!(values::workflow_values(f.pool(), repo.id, kind)
            .await
            .unwrap()
            .is_empty());
        assert!(values::repositories(
            f.pool(),
            owner,
            organizations[0].id,
            kind,
            "SELECTED",
            0,
            100
        )
        .await
        .unwrap()
        .1
        .is_empty());
    }
    f.finish().await;
}

#[tokio::test]
async fn sqlite_organization_values_transfer_revokes_selections() {
    transfer_selections(false).await;
}
#[tokio::test]
async fn postgres_organization_values_transfer_revokes_selections() {
    transfer_selections(true).await;
}

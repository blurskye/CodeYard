use super::*;
use axum::{http::StatusCode, Router};

pub(super) async fn contract(f: &Database, router: &Router, actors: [i64; 3], target: (i64, i64)) {
    let [owner, member, outside] = actors;
    let (org, repo) = target;
    let owner = Client::login(router, f.pool(), owner).await;
    let member = Client::login(router, f.pool(), member).await;
    let outside = Client::login(router, f.pool(), outside).await;
    let base = format!("/settings/organizations/{org}");
    let action = format!("{base}/properties");
    let response = owner
        .form_reply(
            &action,
            &[
                ("action", "save-property"),
                ("property_name", "native_tags"),
                ("value_type", "multi_select"),
                ("allowed_values", "one\n<b>two</b>"),
                ("description", "Native <b>metadata</b>"),
                ("use_default", "true"),
                ("default_value", "one"),
                ("values_editable_by", "org_and_repo_actors"),
            ],
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "{}",
        response.text()
    );
    let id=f.scalar("SELECT id FROM organization_repository_properties WHERE org_id=$1 AND property_name='native_tags'",&[org]).await.to_string();
    let selected = format!("{base}?tab=properties&property=native_tags");
    let (status, html) = owner.page(&selected).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!html.contains("<b>two</b>"));
    let textarea = html
        .split("id=\"property-choices\"")
        .nth(1)
        .unwrap()
        .split("</textarea>")
        .next()
        .unwrap();
    let text = textarea.split_once('>').unwrap().1;
    assert!(
        text.contains("one\n"),
        "native textarea must contain editable SSR text"
    );
    assert!(html.contains("name=\"values_editable_by\""));
    let (status, html) = member.page(&selected).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Allowed choices"));
    assert!(!html.contains("value=\"save-property\""));
    assert!(!html.contains("value=\"delete-property\""));
    let response = member
        .form_reply(
            &action,
            &[
                ("action", "delete-property"),
                ("property_name", "native_tags"),
                ("definition_id", &id),
                ("confirm", "native_tags"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    let response = owner
        .form_reply(
            &action,
            &[
                ("action", "set-property-value"),
                ("property_name", "native_tags"),
                ("repository_names", "first\nsecond"),
                ("value", "not-a-choice"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.text().contains("not-a-choice"));
    assert!(response.text().contains("name=\"repository_names\""));
    let response=owner.exchange("POST",&action,"application/x-www-form-urlencoded",b"csrf_token=incorrect&action=delete-property&property_name=native_tags&confirm=native_tags".to_vec()).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    let response = owner
        .form_reply(
            &action,
            &[
                ("action", "delete-property"),
                ("property_name", "native_tags"),
                ("confirm", "wrong"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.text().contains("value=\"wrong\""));
    let response = owner
        .form_reply(
            &action,
            &[
                ("action", "save-property"),
                ("property_name", "native_tags"),
                ("property_name", "other"),
                ("value_type", "string"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    let settings = "/acme/first/settings?tab=properties&property=native_tags";
    let (status, html) = outside.page(settings).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Save explicit value"));
    let repo = repo.to_string();
    let response = outside
        .form_reply(
            "/acme/first/_forms/repository-property-set",
            &[
                ("repository_id", &repo),
                ("definition_id", &id),
                ("property_name", "native_tags"),
                ("value", "<b>two</b>"),
            ],
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "{}",
        response.text()
    );
    let metadata = properties::repository(f.pool(), Some(actors[2]), target.1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        metadata
            .values
            .properties
            .iter()
            .find(|row| row.property_name == "native_tags")
            .unwrap()
            .value,
        Some(PropertyValue::Strings(vec!["<b>two</b>".into()]))
    );
    let response = outside
        .form_reply(
            "/acme/first/_forms/repository-property-set",
            &[
                ("repository_id", &repo),
                ("definition_id", &id),
                ("property_name", "native_tags"),
                ("value", "wrong"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.text().contains("wrong"));
    let response = outside
        .form_reply(
            "/acme/first/_forms/repository-property-unset",
            &[
                ("repository_id", &repo),
                ("definition_id", &id),
                ("property_name", "native_tags"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let response = outside
        .form_reply(
            "/acme/first/_forms/repository-property-set",
            &[
                ("repository_id", "999999"),
                ("definition_id", &id),
                ("property_name", "native_tags"),
                ("value", "one"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    let protected=f.scalar("SELECT id FROM organization_repository_properties WHERE org_id=$1 AND property_name='env'",&[org]).await.to_string();
    let response = outside
        .form_reply(
            "/acme/first/_forms/repository-property-set",
            &[
                ("repository_id", &repo),
                ("definition_id", &protected),
                ("property_name", "env"),
                ("value", "prod"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    let response = owner
        .form_reply(
            &action,
            &[
                ("action", "delete-property"),
                ("property_name", "native_tags"),
                ("definition_id", &id),
                ("confirm", "native_tags"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let response = owner
        .form_reply(
            &action,
            &[
                ("action", "save-property"),
                ("property_name", "native_tags"),
                ("value_type", "string"),
                ("values_editable_by", "org_and_repo_actors"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let response = outside
        .form_reply(
            "/acme/first/_forms/repository-property-set",
            &[
                ("repository_id", &repo),
                ("definition_id", &id),
                ("property_name", "native_tags"),
                ("value", "stale"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    // A delete form for the old definition leaves its same-named replacement alone.
    let response = owner
        .form_reply(
            &action,
            &[
                ("action", "delete-property"),
                ("property_name", "native_tags"),
                ("definition_id", &id),
                ("confirm", "native_tags"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM organization_repository_properties WHERE org_id=$1 AND property_name='native_tags'",
            &[org]
        )
        .await,
        1
    );

    let stale = properties::set_repository_value(
        f.pool(),
        actors[0],
        target.1,
        id.parse().unwrap(),
        &ValueInput {
            property_name: "native_tags".into(),
            value: Some(PropertyValue::String("stale".into())),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        stale.downcast_ref::<properties::Refusal>(),
        Some(properties::Refusal::Missing)
    ));
    assert!(properties::repository(f.pool(), Some(actors[0]), target.1)
        .await
        .unwrap()
        .unwrap()
        .values
        .properties
        .iter()
        .find(|row| row.property_name == "native_tags")
        .unwrap()
        .value
        .is_none());

    // Creation renders mandatory inputs without scripts and accepts bounded multi-value bodies larger than 16 KiB.
    let response = owner
        .form_reply(
            &action,
            &[
                ("action", "save-property"),
                ("property_name", "native_required"),
                ("value_type", "string"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let response = owner
        .form_reply(
            &action,
            &[
                ("action", "set-property-value"),
                ("property_name", "native_required"),
                ("repository_names", "first\nsecond\nfuture"),
                ("value", "ready"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let response = owner
        .form_reply(
            &action,
            &[
                ("action", "save-property"),
                ("property_name", "native_required"),
                ("value_type", "string"),
                ("required", "true"),
                ("require_explicit_values", "true"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let large = (0..40)
        .map(|index| format!("{index:03}{}", "a".repeat(509)))
        .collect::<Vec<_>>();
    let lines = large.join("\n");
    let response = owner
        .form_reply(
            &action,
            &[
                ("action", "save-property"),
                ("property_name", "native_large"),
                ("value_type", "multi_select"),
                ("allowed_values", &lines),
            ],
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "{}",
        response.text()
    );
    let required_id=f.scalar("SELECT id FROM organization_repository_properties WHERE org_id=$1 AND property_name='native_required'",&[org]).await.to_string();
    let org = org.to_string();
    let (status, html) = owner.page(&format!("/new?owner_org_id={org}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("name=\"property_value_native_required\""));
    assert!(html.contains("name=\"property_set_native_required\""));
    let response = owner
        .form_reply(
            "/new",
            &[
                ("name", "failed-native-metadata"),
                ("owner_org_id", &org),
                ("properties_owner_org_id", &org),
                ("visibility", "private"),
                ("property_set_native_required", "true"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repositories WHERE name='failed-native-metadata'",
            &[]
        )
        .await,
        0
    );
    let response = owner
        .form_reply(
            "/new",
            &[
                ("name", "native-created"),
                ("owner_org_id", &org),
                ("properties_owner_org_id", &org),
                ("visibility", "private"),
                ("property_set_native_required", "true"),
                ("property_value_native_required", "created"),
                ("property_set_native_large", "true"),
                ("property_value_native_large", &lines),
            ],
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "{}",
        response.text()
    );
    let created = f
        .scalar(
            "SELECT id FROM repositories WHERE owner_org_id=$1 AND name='native-created'",
            &[target.0],
        )
        .await;
    let metadata = properties::repository(f.pool(), Some(actors[0]), created)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        metadata
            .values
            .properties
            .iter()
            .find(|row| row.property_name == "native_large")
            .unwrap()
            .value,
        Some(PropertyValue::Strings(large))
    );
    let response = owner
        .form_reply(
            "/new",
            &[
                ("name", "wrong-owner-metadata"),
                ("owner_org_id", &org),
                ("properties_owner_org_id", "999999"),
                ("visibility", "private"),
                ("property_set_native_required", "true"),
                ("property_value_native_required", "other-owner"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repositories WHERE name='wrong-owner-metadata'",
            &[]
        )
        .await,
        0
    );
    let response = owner
        .form_reply(
            "/new",
            &[
                ("intent", "load-properties"),
                ("name", "draft-metadata"),
                ("description", "retained description"),
                ("owner_org_id", &org),
                ("properties_owner_org_id", &org),
                ("visibility", "private"),
                ("property_set_native_required", "true"),
                ("property_value_native_required", "retained-value"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(response.text().contains("value=\"retained-value\""));
    assert!(response.text().contains("retained description"));
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repositories WHERE name='draft-metadata'",
            &[]
        )
        .await,
        0
    );
    let response = owner
        .form_reply(
            &action,
            &[
                ("action", "delete-property"),
                ("property_name", "native_required"),
                ("definition_id", &required_id),
                ("confirm", "native_required"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
}

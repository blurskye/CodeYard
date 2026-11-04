use super::*;
fn definition(value: Value) -> properties::DefinitionInput {
    serde_json::from_value(value).unwrap()
}
pub(super) async fn contract(
    f: &Database,
    owner: i64,
    source: i64,
    destination: i64,
    repos: [i64; 2],
) {
    let initial = [definition(
        json!({"property_name":"race","value_type":"single_select","allowed_values":["a","b"]}),
    )];
    let definitions = applied(
        properties::define(f.pool(), owner, source, &initial)
            .await
            .unwrap(),
    );
    let race = definitions[0].id;
    applied(
        properties::set_values(
            f.pool(),
            owner,
            source,
            &["first".into(), "second".into()],
            &[ValueInput {
                property_name: "race".into(),
                value: Some(PropertyValue::String("a".into())),
            }],
        )
        .await
        .unwrap(),
    );
    let narrow = [definition(
        json!({"property_name":"race","value_type":"single_select","allowed_values":["a"]}),
    )];
    let input = ValueInput {
        property_name: "race".into(),
        value: Some(PropertyValue::String("b".into())),
    };
    let (updated, written) = tokio::join!(
        properties::define(f.pool(), owner, source, &narrow),
        properties::set_repository_value(f.pool(), owner, repos[0], race, &input)
    );
    assert_ne!(
        matches!(updated, Ok(organizations::OrganizationOutcome::Applied(_))),
        written.is_ok(),
        "a definition change and an incompatible value write cannot both commit"
    );
    let current = properties::repository(f.pool(), Some(owner), repos[0])
        .await
        .unwrap()
        .unwrap();
    let schema = &current
        .definitions
        .iter()
        .find(|row| row.id == race)
        .unwrap()
        .fields;
    let value = current
        .values
        .properties
        .iter()
        .find(|row| row.property_name == "race")
        .unwrap()
        .value
        .as_ref()
        .unwrap();
    schema.validate_value(value).unwrap();
    // Independent repository writes must not deadlock while upgrading a shared membership lock.
    let input = ValueInput {
        property_name: "race".into(),
        value: Some(PropertyValue::String("a".into())),
    };
    let writes = async {
        tokio::join!(
            properties::set_repository_value(f.pool(), owner, repos[0], race, &input),
            properties::set_repository_value(f.pool(), owner, repos[1], race, &input)
        )
    };
    let (left, right) = tokio::time::timeout(std::time::Duration::from_secs(15), writes)
        .await
        .expect("repository metadata writes make progress");
    left.unwrap();
    right.unwrap();

    let optional = [definition(
        json!({"property_name":"destination_required","value_type":"string"}),
    )];
    applied(
        properties::define(f.pool(), owner, destination, &optional)
            .await
            .unwrap(),
    );
    applied(
        properties::set_values(
            f.pool(),
            owner,
            destination,
            &["secret".into()],
            &[ValueInput {
                property_name: "destination_required".into(),
                value: Some(PropertyValue::String("ready".into())),
            }],
        )
        .await
        .unwrap(),
    );
    let mandatory = [definition(
        json!({"property_name":"destination_required","value_type":"string","required":true,"require_explicit_values":true}),
    )];
    applied(
        properties::define(f.pool(), owner, destination, &mandatory)
            .await
            .unwrap(),
    );
    let count = f
        .scalar(
            "SELECT COUNT(*) FROM repository_property_values WHERE repo_id=$1",
            &[repos[0]],
        )
        .await;
    assert!(count > 0);
    codeyard_db_repos::repositories::records::transfer_repository(
        f.pool(),
        owner,
        repos[0],
        RepositoryTarget::Organization(source),
    )
    .await
    .unwrap();
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repository_property_values WHERE repo_id=$1",
            &[repos[0]]
        )
        .await,
        count
    );
    let refused = codeyard_db_repos::repositories::records::transfer_repository(
        f.pool(),
        owner,
        repos[0],
        RepositoryTarget::Organization(destination),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        refused.downcast_ref::<properties::Refusal>(),
        Some(properties::Refusal::Invalid { .. })
    ));
    assert_eq!(
        codeyard_db_repos::repositories::records::get_repository_by_id(f.pool(), repos[0])
            .await
            .unwrap()
            .unwrap()
            .owner_org_id,
        Some(source)
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repository_property_values WHERE repo_id=$1",
            &[repos[0]]
        )
        .await,
        count
    );
    let defaults = [definition(
        json!({"property_name":"destination_required","value_type":"string","required":true,"default_value":"destination"}),
    )];
    let target = applied(
        properties::define(f.pool(), owner, destination, &defaults)
            .await
            .unwrap(),
    );
    codeyard_db_repos::repositories::records::transfer_repository(
        f.pool(),
        owner,
        repos[0],
        RepositoryTarget::Organization(destination),
    )
    .await
    .unwrap();
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repository_property_values WHERE repo_id=$1",
            &[repos[0]]
        )
        .await,
        0
    );
    let current = properties::repository(f.pool(), Some(owner), repos[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.values.repository.owner_org_id, Some(destination));
    assert_eq!(current.values.properties.len(), 1);
    assert_eq!(
        current.values.properties[0].value,
        Some(PropertyValue::String("destination".into()))
    );
    let input = ValueInput {
        property_name: "destination_required".into(),
        value: Some(PropertyValue::String("explicit".into())),
    };
    properties::set_repository_value(f.pool(), owner, repos[0], target[0].id, &input)
        .await
        .unwrap();
    codeyard_db_repos::repositories::records::transfer_repository(
        f.pool(),
        owner,
        repos[0],
        RepositoryTarget::Personal(owner),
    )
    .await
    .unwrap();
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM repository_property_values WHERE repo_id=$1",
            &[repos[0]]
        )
        .await,
        0
    );
    assert!(properties::repository(f.pool(), Some(owner), repos[0])
        .await
        .unwrap()
        .is_none());
    assert!(
        f.scalar(
            "SELECT COUNT(*) FROM repository_property_values WHERE repo_id=$1",
            &[repos[1]]
        )
        .await
            > 0
    );
}

use crate::support::{database::Database, organizations::applied};
use codeyard::backend::db::{
    models::OrgRole,
    organizations::{self, NewOrganization, OrganizationOutcome, OrganizationProfile},
    rulesets::{self, Definition, ListFilter, Owner, Page, SuiteFilter},
};
use serde_json::json;

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let outside = f.actor("outside").await;
    let org = applied(
        organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        organizations::add_organization_member(f.pool(), owner, org.id, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    let scope = Owner::Organization(org.id);
    let input = Definition::parse(&json!({"name":"Release policy","enforcement":"active","bypass_actors":[{"actor_type":"OrganizationAdmin"}],"rules":[{"type":"non_fast_forward"}]})).unwrap();
    assert!(matches!(
        rulesets::create_ruleset(f.pool(), member, scope, &input)
            .await
            .unwrap(),
        OrganizationOutcome::Denied
    ));
    assert!(rulesets::create_ruleset(f.pool(), owner, scope, &input)
        .await
        .is_err());
    let input = input.merge(&json!({"enforcement":"disabled"})).unwrap();
    let rule = applied(
        rulesets::create_ruleset(f.pool(), owner, scope, &input)
            .await
            .unwrap(),
    );
    let page = Page::new(0, 2).unwrap();
    let listed = applied(
        rulesets::list_rulesets(f.pool(), Some(member), scope, &ListFilter::default(), page)
            .await
            .unwrap(),
    );
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].can_edit);
    assert!(listed[0].value.definition.bypass_actors.is_empty());
    assert!(matches!(
        rulesets::get_ruleset(f.pool(), Some(outside), scope, rule.id, false)
            .await
            .unwrap(),
        OrganizationOutcome::Denied
    ));
    let changed = applied(
        rulesets::change_ruleset(f.pool(), owner, scope, rule.id, &json!({"name":"Renamed"}))
            .await
            .unwrap(),
    );
    assert!(changed.updated_at > rule.updated_at);
    let versions = applied(
        rulesets::ruleset_versions(f.pool(), Some(owner), scope, rule.id, page)
            .await
            .unwrap(),
    );
    assert_eq!(versions.value.len(), 2);
    assert_eq!(versions.value[0].definition.name, "Renamed");
    let version = applied(
        rulesets::ruleset_version(f.pool(), Some(member), scope, rule.id, versions.value[1].id)
            .await
            .unwrap(),
    );
    assert_eq!(version.value.definition.name, "Release policy");
    assert!(version.value.definition.bypass_actors.is_empty());
    assert!(rulesets::change_ruleset(
        f.pool(),
        owner,
        scope,
        rule.id,
        &json!({"rules":[{"type":"unknown"}]})
    )
    .await
    .is_err());
    let unchanged = applied(
        rulesets::get_ruleset(f.pool(), Some(owner), scope, rule.id, false)
            .await
            .unwrap(),
    );
    assert_eq!(unchanged.value.definition.rules.len(), 1);
    assert!(applied(
        rulesets::organization_rule_suites(
            f.pool(),
            owner,
            org.id,
            &SuiteFilter {
                since: chrono::Utc::now() - chrono::Duration::days(1),
                reference: None,
                repository_name: None,
                actor_name: None,
                result: None,
                enforcement: None
            },
            page
        )
        .await
        .unwrap()
    )
    .is_empty());
    assert!(matches!(
        rulesets::organization_rule_suite(f.pool(), owner, org.id, 9999)
            .await
            .unwrap(),
        OrganizationOutcome::Missing
    ));
    applied(
        rulesets::remove_ruleset(f.pool(), owner, scope, rule.id)
            .await
            .unwrap(),
    );
    assert!(matches!(
        rulesets::get_ruleset(f.pool(), Some(owner), scope, rule.id, false)
            .await
            .unwrap(),
        OrganizationOutcome::Missing
    ));
    f.finish().await;
}
#[tokio::test]
async fn organization_rulesets_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn organization_rulesets_postgres() {
    contract(true).await;
}

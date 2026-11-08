use codeyard_database::queries::rulesets::matching::RepositoryContext;
use codeyard_database::queries::rulesets::records::Owner;
use codeyard_database::queries::rulesets::BypassMode;
use codeyard_database::queries::rulesets::Definition;
use codeyard_database::queries::rulesets::Enforcement;
use codeyard_database::queries::rulesets::Rule;
use serde_json::json;
use serde_json::Value;

#[test]
fn repository_and_ref_selectors_share_native_globs_and_effective_typed_values() {
    use codeyard_types::workflow_glob::WorkflowMatchBudget;
    let values = std::collections::HashMap::from([
        ("critical".to_owned(), vec!["true".to_owned()]),
        (
            "teams".to_owned(),
            vec!["build".to_owned(), "release".to_owned()],
        ),
    ]);
    let repository = RepositoryContext {
        id: 12,
        name: "production",
        default_branch: "main",
        is_private: true,
        is_archived: false,
        is_fork: false,
        properties: &values,
    };
    let parsed = Definition::parse(&definition()).unwrap();
    let mut budget = WorkflowMatchBudget::new();
    assert!(parsed.matches_repository(&repository, &mut budget).unwrap());
    assert!(parsed
        .matches_reference("refs/heads/release/stable", "main", &mut budget)
        .unwrap());
    assert!(!parsed
        .matches_reference("refs/tags/release/stable", "main", &mut budget)
        .unwrap());
    let mut input = definition();
    input["conditions"] = json!({"repository_property":{"include":[{"name":"critical","property_values":["true"]},{"name":"teams","property_values":["release"]},{"name":"visibility","source":"system","property_values":["private"]}],"exclude":[{"name":"archived","source":"system","property_values":["true"]}]},"ref_name":{"include":["~DEFAULT_BRANCH"]}});
    let parsed = Definition::parse(&input).unwrap();
    assert!(parsed.matches_repository(&repository, &mut budget).unwrap());
    assert!(parsed
        .matches_reference("refs/heads/main", "main", &mut budget)
        .unwrap());
    assert!(!parsed
        .matches_reference("refs/heads/other", "main", &mut budget)
        .unwrap());
    let missing = std::collections::HashMap::new();
    let missing = RepositoryContext {
        properties: &missing,
        ..repository
    };
    assert!(!parsed.matches_repository(&missing, &mut budget).unwrap());
}

fn definition() -> Value {
    json!({"name":"Release policy","enforcement":"active","conditions":{"repository_name":{"include":["~ALL"],"exclude":["sandbox-*"]},"ref_name":{"include":["refs/heads/release/**"],"exclude":[]}},"rules":[{"type":"non_fast_forward"},{"type":"commit_message_pattern","parameters":{"operator":"starts_with","pattern":"PROJ-"}}]})
}
#[test]
fn definition_merge_preserves_omitted_policy_and_revalidates_all_rules() {
    let original = Definition::parse(&definition()).unwrap();
    let changed = original
        .merge(&json!({"name":"Renamed","enforcement":"evaluate"}))
        .unwrap();
    assert_eq!(changed.name, "Renamed");
    assert_eq!(changed.enforcement, Enforcement::Evaluate);
    assert_eq!(changed.rules.len(), 2);
    assert_eq!(changed.conditions, original.conditions);
    assert_eq!(original.enforcement, Enforcement::Active);
    assert!(original.merge(&json!({"target":"tag"})).is_err());
    assert!(original
        .merge(&json!({"bypass_actors":[{"actor_type":"RepositoryRole","actor_id":6}]}))
        .is_err());
    assert_eq!(original.rules.len(), 2);
}
#[test]
fn unknown_and_duplicate_rules_never_become_stored_policy() {
    for rules in [
        json!([{"type":"pretend_security"}]),
        json!([{"type":"deletion"},{"type":"deletion"}]),
        json!([{"type":"update","parameters":{"update_allows_fetch_and_merge":true}}]),
        json!([{"type":"non_fast_forward","skip_everything":true}]),
    ] {
        let mut input = definition();
        input["rules"] = rules;
        assert!(Definition::parse(&input).is_err());
    }
    for name in ["", " ", "unsafe\nlabel"] {
        let mut input = definition();
        input["name"] = json!(name);
        assert!(Definition::parse(&input).is_err());
    }
}
#[test]
fn bypass_is_not_a_grant_and_validates_actor_identity_and_mode() {
    for actors in [
        json!([{"actor_type":"User"}]),
        json!([{"actor_type":"User","actor_id":-1}]),
        json!([{"actor_type":"Team","actor_id":2},{"actor_type":"Team","actor_id":2}]),
        json!([{"actor_type":"DeployKey","actor_id":2}]),
        json!([{"actor_type":"DeployKey","bypass_mode":"pull_request"}]),
        json!([{"actor_type":"RepositoryRole","actor_id":6}]),
    ] {
        let mut input = definition();
        input["bypass_actors"] = actors;
        assert!(Definition::parse(&input).is_err());
    }
    let mut input = definition();
    input["bypass_actors"] = json!([{"actor_type":"OrganizationAdmin","actor_id":999},{"actor_type":"RepositoryRole","actor_id":5,"bypass_mode":"pull_request"},{"actor_type":"DeployKey","actor_id":null,"bypass_mode":"exempt"}]);
    let parsed = Definition::parse(&input).unwrap();
    assert_eq!(parsed.bypass_actors[0].actor_id, None);
    assert_eq!(parsed.bypass_actors[1].bypass_mode, BypassMode::PullRequest);
}
#[test]
fn selectors_are_typed_bounded_and_target_specific() {
    let refs = json!({"include":["~DEFAULT_BRANCH"],"exclude":[]});
    for condition in [
        json!({"ref_name":refs}),
        json!({"repository_name":{"include":["~ALL"]},"repository_id":{"repository_ids":[1]},"ref_name":refs}),
        json!({"repository_id":{"repository_ids":[1,1]},"ref_name":refs}),
        json!({"repository_name":{"include":["~DEFAULT_BRANCH"]},"ref_name":refs}),
        json!({"repository_name":{"include":["~ALL"]},"ref_name":{"include":["~UNKNOWN"]}}),
        json!({"repository_property":{"include":[{"name":"tier","property_values":[true]}]},"ref_name":refs}),
    ] {
        let mut input = definition();
        input["conditions"] = condition;
        assert!(Definition::parse(&input)
            .and_then(|input| input.validate_owner(Owner::Organization(1)))
            .is_err());
    }
    let mut input = definition();
    input["conditions"] = json!({"repository_property":{"include":[{"name":"critical","property_values":["true"],"source":"custom"}],"exclude":[]},"ref_name":refs});
    assert!(Definition::parse(&input).is_ok());
    input["target"] = json!("push");
    input["rules"] = json!([]);
    assert!(Definition::parse(&input).is_err());
    input["conditions"]
        .as_object_mut()
        .unwrap()
        .remove("ref_name");
    assert!(Definition::parse(&input).is_ok());
}
#[test]
fn repository_rulesets_accept_refs_but_not_repository_selectors() {
    let mut input = definition();
    input["conditions"] = json!({"ref_name":{"include":["~ALL"]}});
    let parsed = Definition::parse(&input).unwrap();
    assert!(parsed.validate_owner(Owner::Repository(12)).is_ok());
    assert!(parsed.validate_owner(Owner::Organization(1)).is_err());
    assert!(Definition::parse(&definition())
        .unwrap()
        .validate_owner(Owner::Repository(12))
        .is_err());
    assert!(parsed.storage_document(Owner::Repository(12)).is_err());
    input["enforcement"] = json!("disabled");
    assert!(Definition::parse(&input)
        .unwrap()
        .storage_document(Owner::Repository(12))
        .is_ok());
}
#[test]
fn patterns_compile_with_limits_and_rebuild_after_json_transport() {
    let mut input = definition();
    input["rules"] = json!([{"type":"commit_message_pattern","parameters":{"operator":"regex","pattern":"^PROJ-[0-9]+: ","negate":false}}]);
    let parsed = Definition::parse(&input).unwrap();
    let Rule::CommitMessagePattern { parameters } = &parsed.rules[0] else {
        panic!("pattern rule")
    };
    assert!(parameters.matches("PROJ-12: Repair").unwrap());
    assert!(!parameters.matches("example PROJ-12: Repair").unwrap());
    let serialized = serde_json::to_value(&parsed).unwrap();
    let unvalidated: Definition = serde_json::from_value(serialized.clone()).unwrap();
    let Rule::CommitMessagePattern { parameters } = &unvalidated.rules[0] else {
        panic!("pattern rule")
    };
    assert!(parameters.matches("PROJ-12: Repair").is_err());
    let reloaded = Definition::parse(&serialized).unwrap();
    let Rule::CommitMessagePattern { parameters } = &reloaded.rules[0] else {
        panic!("pattern rule")
    };
    assert!(parameters.matches("PROJ-12: Repair").unwrap());
    for pattern in ["[", "(?=unbounded)", ""] {
        input["rules"][0]["parameters"]["pattern"] = json!(pattern);
        assert!(Definition::parse(&input).is_err());
    }
}
#[test]
fn push_limits_require_real_literal_extensions_and_nonzero_bounded_sizes() {
    let mut input = json!({"name":"File policy","target":"push","enforcement":"active","conditions":{"repository_id":{"repository_ids":[12]}},"rules":[{"type":"max_file_size","parameters":{"max_file_size":50}},{"type":"max_file_path_length","parameters":{"max_file_path_length":256}},{"type":"file_extension_restriction","parameters":{"restricted_file_extensions":[".exe",".dll"]}}]});
    assert!(Definition::parse(&input).is_ok());
    input["rules"][2]["parameters"]["restricted_file_extensions"] = json!(["*.exe"]);
    assert!(Definition::parse(&input).is_err());
    input["rules"][2]["parameters"]["restricted_file_extensions"] = json!([".exe"]);
    input["rules"][0]["parameters"]["max_file_size"] = json!(0);
    assert!(Definition::parse(&input).is_err());
}

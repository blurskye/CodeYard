use super::*;
use async_graphql_parser::{
    parse_schema,
    types::{ConstDirective, InputValueDefinition, TypeKind, TypeSystemDefinition},
    Positioned,
};
use serde_json::{json, Value as Json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

struct Unsupported;
impl QueryResolver for Unsupported {}
impl MutationResolver for Unsupported {}

fn schema() -> &'static GitHubSchema {
    static SCHEMA: OnceLock<GitHubSchema> = OnceLock::new();
    SCHEMA.get_or_init(|| GitHubSchema::new(Query::new(Unsupported), Mutation::new(Unsupported)))
}

fn deprecation(directives: &[Positioned<ConstDirective>]) -> Json {
    directives
        .iter()
        .find(|d| d.node.name.node == "deprecated")
        .map(|d| {
            d.node
                .arguments
                .iter()
                .find(|(n, _)| n.node == "reason")
                .map(|(_, v)| json!(v.node.to_string()))
                .unwrap_or_else(|| json!("\"No longer supported\""))
        })
        .unwrap_or(Json::Null)
}

fn input(value: &InputValueDefinition) -> Json {
    json!({"description":value.description.as_ref().map(|v| &v.node), "type":value.ty.node.to_string(), "default":value.default_value.as_ref().map(|v|v.node.to_string()), "deprecated":deprecation(&value.directives)})
}

fn shape(sdl: &str) -> BTreeMap<String, Json> {
    parse_schema(sdl).unwrap().definitions.into_iter().filter_map(|definition| {
        let TypeSystemDefinition::Type(definition) = definition else { return None; };
        let definition = definition.node;
        let details = match definition.kind {
            TypeKind::Scalar => json!({"kind":"scalar"}),
            TypeKind::Object(object) => {
                let fields: BTreeMap<_, _> = object.fields.into_iter().map(|f| {
                    let f = f.node;
                    let args: BTreeMap<_, _> = f.arguments.iter().map(|a| (a.node.name.node.to_string(), input(&a.node))).collect();
                    (f.name.node.to_string(), json!({"description":f.description.map(|d|d.node),"type":f.ty.node.to_string(),"deprecated":deprecation(&f.directives),"args":args}))
                }).collect();
                json!({"kind":"object","interfaces":object.implements.into_iter().map(|i|i.node.to_string()).collect::<BTreeSet<_>>(),"fields":fields})
            }
            TypeKind::Interface(interface) => {
                let fields: BTreeMap<_, _> = interface.fields.into_iter().map(|f| {
                    let f = f.node;
                    let args: BTreeMap<_, _> = f.arguments.iter().map(|a| (a.node.name.node.to_string(), input(&a.node))).collect();
                    (f.name.node.to_string(), json!({"description":f.description.map(|d|d.node),"type":f.ty.node.to_string(),"deprecated":deprecation(&f.directives),"args":args}))
                }).collect();
                json!({"kind":"interface","interfaces":interface.implements.into_iter().map(|i|i.node.to_string()).collect::<BTreeSet<_>>(),"fields":fields})
            }
            TypeKind::Union(union) => json!({"kind":"union","members":union.members.into_iter().map(|m|m.node.to_string()).collect::<BTreeSet<_>>()}),
            TypeKind::Enum(enumeration) => {
                let values: BTreeMap<_, _> = enumeration.values.into_iter().map(|v| (v.node.value.node.to_string(), json!({"description":v.node.description.map(|d|d.node),"deprecated":deprecation(&v.node.directives)}))).collect();
                json!({"kind":"enum","values":values})
            }
            TypeKind::InputObject(object) => {
                let fields: BTreeMap<_, _> = object.fields.iter().map(|f| (f.node.name.node.to_string(), input(&f.node))).collect();
                json!({"kind":"input","fields":fields})
            }
        };
        Some((definition.name.node.to_string(), json!({"description":definition.description.map(|d|d.node),"details":details})))
    }).collect()
}

fn directive_shape(sdl: &str) -> BTreeMap<String, Json> {
    parse_schema(sdl).unwrap().definitions.into_iter().filter_map(|definition| {
        let TypeSystemDefinition::Directive(definition) = definition else { return None; };
        let definition = definition.node;
        let args: BTreeMap<_, _> = definition.arguments.iter().map(|a| (a.node.name.node.to_string(), input(&a.node))).collect();
        let locations: BTreeSet<_> = definition.locations.into_iter().map(|l| format!("{:?}", l.node)).collect();
        Some((definition.name.node.to_string(), json!({"description":definition.description.map(|d|d.node),"args":args,"locations":locations,"repeatable":definition.is_repeatable})))
    }).collect()
}

#[test]
fn exported_derive_schema_matches_every_sdl_type_and_field() {
    let expected = shape(include_str!("../../../../spec/github/schema.docs.graphql"));
    let exported = schema().sdl();
    let directives = directive_shape(&exported);
    for (name, expected) in
        directive_shape(include_str!("../../../../spec/github/schema.docs.graphql"))
    {
        assert_eq!(directives[&name], expected, "directive {name}");
    }
    let actual = shape(&exported);
    assert_eq!(
        actual.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>()
    );
    for (name, expected) in expected {
        assert_eq!(actual[&name], expected, "{name}");
    }
}

#[test]
fn default_input_literals_preserve_omitted_fields() {
    let input =
        <Input<ProjectV2Filters> as InputType>::parse(Some(Value::Object(Default::default())))
            .unwrap();
    assert_eq!(input.to_value().to_string(), "{}");
    assert!(input.state.is_none());
}

#[tokio::test]
async fn orphan_unions_stay_hidden_without_introspection() {
    let result = schema()
        .execute(
            Request::new("{ t:__type(name:\"OrganizationOrUser\") { name } }")
                .disable_introspection(),
        )
        .await;
    let data = result.data.into_json().unwrap_or_default();
    assert_ne!(data["t"]["name"], "OrganizationOrUser");
}

#[tokio::test]
async fn orphan_union_introspection_honors_aliases_fragments_and_variables() {
    let result = schema().execute(Request::new("query($type:String! = \"OrganizationOrUser\", $members:Boolean! = true) { value:__type(name:$type) { ...Summary possibleTypes @include(if:$members) { ...Summary fields { name type { kind name } } } } __schema { types { name } } } fragment Summary on __Type { label:name kind description }")).await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let data = result.data.into_json().unwrap();
    assert_eq!(data["value"]["label"], "OrganizationOrUser");
    assert_eq!(data["value"]["kind"], "UNION");
    let members: BTreeSet<_> = data["value"]["possibleTypes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["label"].as_str().unwrap())
        .collect();
    assert_eq!(members, BTreeSet::from(["Organization", "User"]));
    assert!(
        data["value"]["possibleTypes"][0]["fields"]
            .as_array()
            .unwrap()
            .len()
            > 10
    );
    assert!(data["__schema"]["types"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["name"] == "OrganizationOrUser"));
    let result = schema().execute(Request::new("query($show:Boolean! = true) { visible:__type(name:\"User\") @include(if:$show) { name } hidden:__type(name:\"User\") @skip(if:$show) { name } }")).await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let data = result.data.into_json().unwrap();
    assert_eq!(data["visible"]["name"], "User");
    assert!(data.get("hidden").is_none());
    let named = "query First($show:Boolean! = false) { kind:__typename value:__type(name:\"User\") @include(if:$show) { name } } query Second($show:Boolean! = true) { kind:__typename value:__type(name:\"User\") @include(if:$show) { name } }";
    let result = schema()
        .execute(Request::new(named).operation_name("Second"))
        .await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.data.into_json().unwrap()["value"]["name"], "User");
    let result = schema()
        .execute(
            Request::new(named)
                .operation_name("Second")
                .variables(async_graphql::Variables::from_json(json!({"show":false}))),
        )
        .await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.data.into_json().unwrap(), json!({"kind":"Query"}));
}

#[test]
fn date_inputs_are_checked_while_parsing() {
    use async_graphql::ScalarType;
    let date = |text: &str| <Date as ScalarType>::parse(Value::String(text.to_owned())).is_ok();
    assert!(date("2024-02-29"));
    assert!(date("2024-01-01T00:00:00Z"));
    assert!(!date("2023-02-29"));
    assert!(!date("not-a-date"));
    assert!(<Date as ScalarType>::parse(Value::Number(1.into())).is_err());
}

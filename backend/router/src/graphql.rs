//! Plan only top-level GraphQL dispatch; nested selections stay in their parent's cell.
use anyhow::{ensure, Result};
use async_graphql::{
    parser::{
        types::{
            Directive, DocumentOperations, ExecutableDocument, Field, OperationDefinition,
            OperationType, Selection, SelectionSet,
        },
        Positioned,
    },
    ServerError, Value,
};
use async_graphql_value::Value as InputValue;
use serde_json::Value as Json;
use std::{collections::BTreeSet, fmt::Write};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    Name(String),
    Node(String),
    Nodes(Vec<String>),
    Credential,
    Global,
}
#[derive(Clone, Debug)]
pub struct RootField {
    pub key: String,
    pub target: Target,
}

pub struct Plan {
    document: ExecutableDocument,
    operation_name: Option<String>,
    variables: async_graphql::Variables,
    pub fields: Vec<RootField>,
    pub mutation: bool,
}
#[derive(Debug)]
pub enum Invalid {
    Request,
    Errors(Vec<ServerError>),
}

impl Plan {
    pub async fn parse(body: Json) -> std::result::Result<Self, Invalid> {
        let mut request = crate::request::parse_request(body).map_err(|_| Invalid::Request)?;
        let document = request
            .parsed_query()
            .map_err(|error| Invalid::Errors(vec![error]))?
            .clone();
        let mut operation_name = request.operation_name.clone();
        let variables = request.variables.clone();
        let errors = crate::validation::validate_request(request).await;
        if !errors.is_empty() {
            return Err(Invalid::Errors(errors));
        }
        if operation_name.is_none() {
            if let DocumentOperations::Multiple(operations) = &document.operations {
                if operations.len() == 1 {
                    operation_name = operations.keys().next().map(ToString::to_string);
                }
            }
        }
        let operation = operation(&document, operation_name.as_deref()).ok_or(Invalid::Request)?;
        if operation.ty == OperationType::Subscription {
            return Err(Invalid::Request);
        }
        let mut selected = Vec::new();
        collect(
            &document,
            operation,
            &operation.selection_set.node,
            &variables,
            0,
            &mut selected,
        )
        .map_err(|_| Invalid::Request)?;
        let mut fields = Vec::new();
        let mut seen = BTreeSet::new();
        for field in selected {
            let key = field.response_key().node.as_str();
            if !seen.insert(key) {
                continue;
            }
            if key.len() > 4096 {
                return Err(Invalid::Request);
            }
            let target = target(field, operation, &variables).map_err(|_| Invalid::Request)?;
            fields.push(RootField {
                key: key.to_owned(),
                target,
            });
        }
        let mutation = operation.ty == OperationType::Mutation;
        Ok(Self {
            document,
            operation_name,
            variables,
            fields,
            mutation,
        })
    }

    /// Keep other fields present under @skip wrappers. This retains variable and
    /// fragment use, so splitting introduces no unused-variable validation errors.
    pub fn request(&self, selected: &BTreeSet<String>) -> Result<Json> {
        let mut text = String::new();
        for (name, definition) in self.document.operations.iter() {
            let definition = &definition.node;
            write!(text, "{}", definition.ty)?;
            if let Some(name) = name {
                write!(text, " {name}")?;
            }
            if !definition.variable_definitions.is_empty() {
                text.push('(');
                for variable in &definition.variable_definitions {
                    let variable = &variable.node;
                    write!(text, "${}:{}", variable.name.node, variable.var_type.node)?;
                    if let Some(value) = &variable.default_value {
                        write!(text, "={}", value.node)?;
                    }
                    directives(&mut text, &variable.directives)?;
                    text.push(' ');
                }
                text.push(')');
            }
            directives(&mut text, &definition.directives)?;
            let chosen = name.map(|name| name.as_str()) == self.operation_name.as_deref();
            selection(
                &mut text,
                &definition.selection_set.node,
                if chosen { Some(selected) } else { None },
                0,
            )?;
        }
        for (name, definition) in &self.document.fragments {
            let definition = &definition.node;
            write!(
                text,
                " fragment {name} on {}",
                definition.type_condition.node.on.node
            )?;
            directives(&mut text, &definition.directives)?;
            let root = matches!(
                definition.type_condition.node.on.node.as_str(),
                "Query" | "Mutation"
            );
            selection(
                &mut text,
                &definition.selection_set.node,
                root.then_some(selected),
                0,
            )?;
        }
        Ok(
            serde_json::json!({"query":text,"operationName":self.operation_name,"variables":self.variables}),
        )
    }
}

fn operation<'a>(
    document: &'a ExecutableDocument,
    name: Option<&str>,
) -> Option<&'a OperationDefinition> {
    match &document.operations {
        DocumentOperations::Single(operation) if name.is_none() => Some(&operation.node),
        DocumentOperations::Multiple(operations) => name
            .and_then(|name| operations.get(name))
            .or_else(|| {
                (operations.len() == 1)
                    .then(|| operations.values().next())
                    .flatten()
            })
            .map(|operation| &operation.node),
        _ => None,
    }
}
fn constant(
    value: &InputValue,
    operation: &OperationDefinition,
    variables: &async_graphql::Variables,
) -> Result<async_graphql::Value> {
    value.clone().into_const_with(|name| {
        variables
            .get(&name)
            .cloned()
            .or_else(|| {
                operation
                    .variable_definitions
                    .iter()
                    .find(|variable| variable.node.name.node == name)
                    .and_then(|variable| variable.node.default_value().cloned())
            })
            .ok_or_else(|| anyhow::anyhow!("Missing variable"))
    })
}
fn included(
    directives: &[Positioned<Directive>],
    operation: &OperationDefinition,
    variables: &async_graphql::Variables,
) -> Result<bool> {
    for directive in directives {
        let name = directive.node.name.node.as_str();
        if !matches!(name, "skip" | "include") {
            continue;
        }
        let value = &directive
            .node
            .arguments
            .iter()
            .find(|(name, _)| name.node == "if")
            .ok_or_else(|| anyhow::anyhow!("Missing directive condition"))?
            .1
            .node;
        let Value::Boolean(condition) = constant(value, operation, variables)? else {
            anyhow::bail!("Invalid directive condition");
        };
        if (name == "skip" && condition) || (name == "include" && !condition) {
            return Ok(false);
        }
    }
    Ok(true)
}
fn collect<'a>(
    document: &'a ExecutableDocument,
    operation: &OperationDefinition,
    set: &'a SelectionSet,
    variables: &async_graphql::Variables,
    depth: usize,
    fields: &mut Vec<&'a Field>,
) -> Result<()> {
    ensure!(
        depth <= 64 && fields.len() <= 1000,
        "Query routing limit exceeded"
    );
    for item in &set.items {
        if !included(item.node.directives(), operation, variables)? {
            continue;
        }
        match &item.node {
            Selection::Field(field) => fields.push(&field.node),
            Selection::InlineFragment(fragment) => collect(
                document,
                operation,
                &fragment.node.selection_set.node,
                variables,
                depth + 1,
                fields,
            )?,
            Selection::FragmentSpread(fragment) => {
                let fragment = document
                    .fragments
                    .get(&fragment.node.fragment_name.node)
                    .ok_or_else(|| anyhow::anyhow!("Unknown fragment"))?;
                collect(
                    document,
                    operation,
                    &fragment.node.selection_set.node,
                    variables,
                    depth + 1,
                    fields,
                )?;
            }
        }
    }
    ensure!(fields.len() <= 1000, "Query routing limit exceeded");
    Ok(())
}
fn argument(
    field: &Field,
    name: &str,
    operation: &OperationDefinition,
    variables: &async_graphql::Variables,
) -> Result<Option<Value>> {
    field
        .get_argument(name)
        .map(|value| constant(&value.node, operation, variables))
        .transpose()
}
fn string(value: Value) -> Result<String> {
    let Value::String(value) = value else {
        anyhow::bail!("Expected a routing string");
    };
    ensure!(value.len() <= 4096, "Routing argument too long");
    Ok(value)
}
fn target(
    field: &Field,
    operation: &OperationDefinition,
    variables: &async_graphql::Variables,
) -> Result<Target> {
    match field.name.node.as_str() {
        "repository" => {
            return Ok(Target::Name(string(
                argument(field, "owner", operation, variables)?
                    .ok_or_else(|| anyhow::anyhow!("Missing owner"))?,
            )?))
        }
        "user" | "organization" | "repositoryOwner" => {
            return Ok(Target::Name(string(
                argument(field, "login", operation, variables)?
                    .ok_or_else(|| anyhow::anyhow!("Missing login"))?,
            )?))
        }
        "node" => {
            return Ok(Target::Node(string(
                argument(field, "id", operation, variables)?
                    .ok_or_else(|| anyhow::anyhow!("Missing node"))?,
            )?))
        }
        "nodes" => {
            let Some(Value::List(ids)) = argument(field, "ids", operation, variables)? else {
                anyhow::bail!("Invalid nodes");
            };
            ensure!(ids.len() <= 1000, "Too many nodes");
            return Ok(Target::Nodes(
                ids.into_iter().map(string).collect::<Result<_>>()?,
            ));
        }
        "search" | "license" | "licenses" | "securityAdvisories" => return Ok(Target::Global),
        _ => {}
    }
    if operation.ty == OperationType::Mutation {
        if let Some(Value::Object(input)) = argument(field, "input", operation, variables)? {
            for name in [
                "repositoryId",
                "projectId",
                "projectV2Id",
                "organizationId",
                "teamId",
                "issueId",
                "pullRequestId",
                "discussionId",
                "commentId",
                "subjectId",
                "id",
                "userId",
            ] {
                if let Some(Value::String(value)) = input.get(name) {
                    ensure!(value.len() <= 4096, "Routing ID too long");
                    return Ok(Target::Node(value.clone()));
                }
            }
            for (name, value) in &input {
                if name.as_str().ends_with("Id")
                    && !matches!(name.as_str(), "clientMutationId" | "actorId" | "authorId")
                {
                    if let Value::String(value) = value {
                        ensure!(value.len() <= 4096, "Routing ID too long");
                        return Ok(Target::Node(value.clone()));
                    }
                }
            }
        }
    }
    Ok(Target::Credential)
}
fn directives(text: &mut String, values: &[Positioned<Directive>]) -> Result<()> {
    for directive in values {
        write!(text, " @{}", directive.node.name.node)?;
        if !directive.node.arguments.is_empty() {
            text.push('(');
            for (name, value) in &directive.node.arguments {
                write!(text, "{}:{} ", name.node, value.node)?;
            }
            text.push(')');
        }
    }
    Ok(())
}
fn selection(
    text: &mut String,
    set: &SelectionSet,
    selected: Option<&BTreeSet<String>>,
    depth: usize,
) -> Result<()> {
    ensure!(depth <= 64, "Query nesting limit exceeded");
    text.push('{');
    for item in &set.items {
        match &item.node {
            Selection::Field(field) => {
                let field = &field.node;
                let skipped = selected
                    .is_some_and(|selected| !selected.contains(field.response_key().node.as_str()));
                if skipped {
                    text.push_str("... @skip(if:true){");
                }
                if let Some(alias) = &field.alias {
                    write!(text, "{}:", alias.node)?;
                }
                write!(text, "{}", field.name.node)?;
                if !field.arguments.is_empty() {
                    text.push('(');
                    for (name, value) in &field.arguments {
                        write!(text, "{}:{} ", name.node, value.node)?;
                    }
                    text.push(')');
                }
                directives(text, &field.directives)?;
                if !field.selection_set.node.items.is_empty() {
                    selection(text, &field.selection_set.node, None, depth + 1)?;
                }
                if skipped {
                    text.push('}');
                }
            }
            Selection::FragmentSpread(fragment) => {
                write!(text, "...{}", fragment.node.fragment_name.node)?;
                directives(text, &fragment.node.directives)?;
            }
            Selection::InlineFragment(fragment) => {
                text.push_str("...");
                if let Some(condition) = &fragment.node.type_condition {
                    write!(text, " on {}", condition.node.on.node)?;
                }
                directives(text, &fragment.node.directives)?;
                selection(text, &fragment.node.selection_set.node, selected, depth + 1)?;
            }
        }
        text.push(' ');
    }
    text.push('}');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn splitting_keeps_fragments_variables_aliases_and_directives_valid() {
        let body = json!({"query":r#"query Across($owner:String!,$skip:Boolean!){
            ...Pair
            extra:repository(owner:$owner,name:"repo") @skip(if:$skip){id}
        } fragment Pair on Query {
            left:repository(owner:"alice",name:"repo"){id}
            right:repository(owner:"bob",name:"repo"){id}
        }"#,"variables":{"owner":"carol","skip":false}});
        let plan = Plan::parse(body).await.unwrap();
        assert!(!plan.mutation);
        assert_eq!(
            plan.fields
                .iter()
                .map(|field| field.key.as_str())
                .collect::<Vec<_>>(),
            ["left", "right", "extra"]
        );
        assert_eq!(plan.fields[0].target, Target::Name("alice".into()));
        assert_eq!(plan.fields[1].target, Target::Name("bob".into()));
        let one = plan.request(&BTreeSet::from(["left".to_owned()])).unwrap();
        let one = Plan::parse(one).await.unwrap();
        assert_eq!(one.fields.len(), 1);
        assert_eq!(one.fields[0].key, "left");
        let both = plan
            .request(&BTreeSet::from(["left".to_owned(), "right".to_owned()]))
            .unwrap();
        let both = Plan::parse(both).await.unwrap();
        assert_eq!(
            both.fields
                .iter()
                .map(|field| field.key.as_str())
                .collect::<Vec<_>>(),
            ["left", "right"]
        );
    }

    #[tokio::test]
    async fn mutation_planning_preserves_order_and_rejects_any_invalid_field_first() {
        let source = r#"mutation{first:createIssue(input:{repositoryId:"R_AAAAAAAAAAE",title:"first"}){issue{id}}
            second:createIssue(input:{repositoryId:"R_AAAAAAAAAAI",title:"second"}){issue{id}}}"#;
        let plan = Plan::parse(json!({"query":source})).await.unwrap();
        assert!(plan.mutation);
        assert_eq!(
            plan.fields
                .iter()
                .map(|field| field.key.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(plan.fields[1].target, Target::Node("R_AAAAAAAAAAI".into()));
        let invalid = source.replacen("second:createIssue", "second:unknownMutation", 1);
        assert!(
            matches!(Plan::parse(json!({"query":invalid})).await,Err(Invalid::Errors(errors)) if !errors.is_empty())
        );
        let conflict = r#"query{a:repository(owner:"alice",name:"r"){id} a:repository(owner:"bob",name:"r"){id}}"#;
        assert!(
            matches!(Plan::parse(json!({"query":conflict})).await,Err(Invalid::Errors(errors)) if !errors.is_empty())
        );
    }

    #[tokio::test]
    async fn skipped_targets_are_never_routed_and_parser_errors_keep_graphql_shape() {
        let plan=Plan::parse(json!({"query":"query($skip:Boolean=true){repository(owner:\"absent\",name:\"r\")@skip(if:$skip){id}}"})).await.unwrap();
        assert!(plan.fields.is_empty());
        assert!(matches!(
            Plan::parse(json!({"query":"query{"})).await,
            Err(Invalid::Errors(_))
        ));
        assert!(matches!(
            Plan::parse(json!({"query":5})).await,
            Err(Invalid::Request)
        ));
    }
}

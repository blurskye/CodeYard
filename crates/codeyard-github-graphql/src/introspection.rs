//! async-graphql 7.2.1 omits explicitly registered orphan unions from introspection.
//! Project those unions from the *derived registry*, not a second SDL schema. All
//! reachable types, validation, execution and error propagation remain upstream.
use async_graphql::{
    extensions::{
        Extension, ExtensionContext, ExtensionFactory, NextParseQuery, NextPrepareRequest,
        NextResolve, ResolveInfo,
    },
    parser::types::{ExecutableDocument, Field, FragmentDefinition, Selection, SelectionSet},
    registry::{
        Deprecation, MetaEnumValue, MetaField, MetaInputValue, MetaType, MetaTypeName, Registry,
    },
    Name, Positioned, Request, ServerResult, Value, Variables,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

pub(crate) struct OrphanUnions;

impl ExtensionFactory for OrphanUnions {
    fn create(&self) -> Arc<dyn Extension> {
        Arc::new(OrphanQuery::default())
    }
}

#[derive(Default)]
struct Query {
    variables: Variables,
    fragments: HashMap<Name, Positioned<FragmentDefinition>>,
    /// A request or schema without introspection gets no orphan types either.
    introspection_disabled: bool,
}

#[derive(Default)]
struct OrphanQuery(Mutex<Query>);

fn constant(value: &async_graphql_value::Value, variables: &Variables) -> Value {
    value
        .clone()
        .into_const_with(|name| Ok::<_, ()>(variables.get(&name).cloned().unwrap_or(Value::Null)))
        .unwrap_or(Value::Null)
}

#[async_graphql::async_trait::async_trait]
impl Extension for OrphanQuery {
    async fn prepare_request(
        &self,
        ctx: &ExtensionContext<'_>,
        request: Request,
        next: NextPrepareRequest<'_>,
    ) -> ServerResult<Request> {
        let mut request = next.run(ctx, request).await?;
        // 7.2.1 prunes @skip/@include using request.variables without consulting
        // operation defaults. Normalize them through the normal cached Request
        // parser before upstream validation/pruning; explicit null still wins.
        let selected = request.operation_name.clone();
        let document = request.parsed_query()?;
        let unambiguous = selected.is_some() || document.operations.iter().count() == 1;
        let defaults: Vec<_> = document
            .operations
            .iter()
            .find(|(name, _)| {
                unambiguous
                    && selected
                        .as_deref()
                        .is_none_or(|selected| name.is_some_and(|name| name.as_str() == selected))
            })
            .into_iter()
            .flat_map(|(_, operation)| &operation.node.variable_definitions)
            .filter_map(|variable| {
                variable
                    .node
                    .default_value
                    .as_ref()
                    .map(|value| (variable.node.name.node.clone(), value.node.clone()))
            })
            .collect();
        for (name, value) in defaults {
            request.variables.entry(name).or_insert(value);
        }
        self.0.lock().unwrap().introspection_disabled = request.introspection_mode
            == async_graphql::IntrospectionMode::Disabled
            || ctx.schema_env.registry.introspection_mode
                == async_graphql::IntrospectionMode::Disabled;
        Ok(request)
    }

    async fn parse_query(
        &self,
        ctx: &ExtensionContext<'_>,
        query: &str,
        variables: &Variables,
        next: NextParseQuery<'_>,
    ) -> ServerResult<ExecutableDocument> {
        let document = next.run(ctx, query, variables).await?;
        let mut state = self.0.lock().unwrap();
        state.variables = variables.clone();
        state.fragments = document.fragments.clone();
        Ok(document)
    }

    async fn resolve(
        &self,
        ctx: &ExtensionContext<'_>,
        info: ResolveInfo<'_>,
        next: NextResolve<'_>,
    ) -> ServerResult<Option<Value>> {
        let schema_types = info.parent_type == "__Schema" && info.name == "types";
        let lookup = info.parent_type == "Query" && info.name == "__type";
        if !schema_types && !lookup {
            return next.run(ctx, info).await;
        }
        let field = info.field;
        let result = next.run(ctx, info).await?;
        let state = self.0.lock().unwrap();
        if state.introspection_disabled {
            return Ok(result);
        }
        let projector = Projector {
            registry: &ctx.schema_env.registry,
            query: &state,
        };
        if schema_types {
            if let Some(Value::List(mut values)) = result {
                for name in crate::schema::ORPHAN_UNIONS {
                    values.push(
                        projector.project(Subject::Type((*name).into()), &field.selection_set.node),
                    );
                }
                return Ok(Some(Value::List(values)));
            }
        } else if let Some(Value::String(name)) = field
            .get_argument("name")
            .map(|v| constant(&v.node, &state.variables))
        {
            if crate::schema::ORPHAN_UNIONS.contains(&name.as_str()) {
                return Ok(Some(
                    projector.project(Subject::Type(name), &field.selection_set.node),
                ));
            }
        }
        Ok(result)
    }
}

#[derive(Clone)]
enum Subject<'a> {
    Type(String),
    Field(&'a MetaField),
    Input(&'a MetaInputValue),
    Enum(&'a MetaEnumValue),
}

impl Subject<'_> {
    fn typename(&self) -> &'static str {
        match self {
            Self::Type(_) => "__Type",
            Self::Field(_) => "__Field",
            Self::Input(_) => "__InputValue",
            Self::Enum(_) => "__EnumValue",
        }
    }
}

enum Datum<'a> {
    Leaf(Value),
    One(Subject<'a>),
    Many(Vec<Subject<'a>>),
}

fn text(value: Option<&str>) -> Datum<'_> {
    Datum::Leaf(
        value
            .map(|s| Value::String(s.into()))
            .unwrap_or(Value::Null),
    )
}
fn boolean(value: bool) -> Datum<'static> {
    Datum::Leaf(Value::Boolean(value))
}
fn deprecated<'a>(value: &'a Deprecation, name: &str) -> Datum<'a> {
    match name {
        "isDeprecated" => boolean(value.is_deprecated()),
        "deprecationReason" => text(value.reason()),
        _ => Datum::Leaf(Value::Null),
    }
}

struct Projector<'a> {
    registry: &'a Registry,
    query: &'a Query,
}

impl<'a> Projector<'a> {
    fn included(&self, selection: &Selection) -> bool {
        selection.directives().iter().all(|directive| {
            let condition = directive
                .node
                .get_argument("if")
                .map(|value| constant(&value.node, &self.query.variables));
            match directive.node.name.node.as_str() {
                "skip" => condition != Some(Value::Boolean(true)),
                "include" => condition == Some(Value::Boolean(true)),
                _ => true,
            }
        })
    }

    fn project(&self, subject: Subject<'a>, selection: &SelectionSet) -> Value {
        let mut object = async_graphql::indexmap::IndexMap::new();
        self.select(&subject, selection, &mut object);
        Value::Object(object)
    }

    fn select(
        &self,
        subject: &Subject<'a>,
        selection: &SelectionSet,
        object: &mut async_graphql::indexmap::IndexMap<Name, Value>,
    ) {
        for item in &selection.items {
            if !self.included(&item.node) {
                continue;
            }
            match &item.node {
                Selection::Field(field) => {
                    let value = if field.node.name.node == "__typename" {
                        Datum::Leaf(Value::String(subject.typename().into()))
                    } else {
                        self.field(subject, &field.node)
                    };
                    let value = match value {
                        Datum::Leaf(value) => value,
                        Datum::One(value) => self.project(value, &field.node.selection_set.node),
                        Datum::Many(values) => Value::List(
                            values
                                .into_iter()
                                .map(|value| self.project(value, &field.node.selection_set.node))
                                .collect(),
                        ),
                    };
                    // Repeated selections merge just as normal GraphQL selections do.
                    let key = field.node.response_key().node.clone();
                    if let Some(previous) = object.get_mut(&key) {
                        merge(previous, value);
                    } else {
                        object.insert(key, value);
                    }
                }
                Selection::InlineFragment(fragment) => {
                    if fragment
                        .node
                        .type_condition
                        .as_ref()
                        .is_none_or(|ty| ty.node.on.node == subject.typename())
                    {
                        self.select(subject, &fragment.node.selection_set.node, object);
                    }
                }
                Selection::FragmentSpread(spread) => {
                    if let Some(fragment) =
                        self.query.fragments.get(&spread.node.fragment_name.node)
                    {
                        if fragment.node.type_condition.node.on.node == subject.typename() {
                            self.select(subject, &fragment.node.selection_set.node, object);
                        }
                    }
                }
            }
        }
    }

    fn field(&self, subject: &Subject<'a>, field: &Field) -> Datum<'a> {
        let name = field.name.node.as_str();
        let include = field
            .get_argument("includeDeprecated")
            .is_some_and(|v| constant(&v.node, &self.query.variables) == Value::Boolean(true));
        match subject {
            Subject::Type(ty) => self.type_field(ty, name, include),
            Subject::Field(value) => match name {
                "name" => text(Some(&value.name)),
                "description" => text(value.description.as_deref()),
                "type" => Datum::One(Subject::Type(value.ty.clone())),
                "args" => Datum::Many(
                    value
                        .args
                        .values()
                        .filter(|v| include || !v.deprecation.is_deprecated())
                        .map(Subject::Input)
                        .collect(),
                ),
                _ => deprecated(&value.deprecation, name),
            },
            Subject::Input(value) => match name {
                "name" => text(Some(&value.name)),
                "description" => text(value.description.as_deref()),
                "type" => Datum::One(Subject::Type(value.ty.clone())),
                "defaultValue" => text(value.default_value.as_deref()),
                _ => deprecated(&value.deprecation, name),
            },
            Subject::Enum(value) => match name {
                "name" => text(Some(&value.name)),
                "description" => text(value.description.as_deref()),
                _ => deprecated(&value.deprecation, name),
            },
        }
    }

    fn type_field(&self, ty: &str, name: &str, include: bool) -> Datum<'a> {
        let parsed = MetaTypeName::create(ty);
        match parsed {
            MetaTypeName::NonNull(inner) | MetaTypeName::List(inner) => {
                return match name {
                    "kind" => text(Some(if matches!(parsed, MetaTypeName::NonNull(_)) {
                        "NON_NULL"
                    } else {
                        "LIST"
                    })),
                    "ofType" => Datum::One(Subject::Type(inner.into())),
                    _ => Datum::Leaf(Value::Null),
                }
            }
            MetaTypeName::Named(_) => {}
        }
        let Some(meta) = self.registry.types.get(ty) else {
            return Datum::Leaf(Value::Null);
        };
        match name {
            "name" => text(Some(meta.name())),
            "kind" => text(Some(match meta {
                MetaType::Scalar { .. } => "SCALAR",
                MetaType::Object { .. } => "OBJECT",
                MetaType::Interface { .. } => "INTERFACE",
                MetaType::Union { .. } => "UNION",
                MetaType::Enum { .. } => "ENUM",
                MetaType::InputObject { .. } => "INPUT_OBJECT",
            })),
            "description" => text(match meta {
                MetaType::Scalar { description, .. }
                | MetaType::Object { description, .. }
                | MetaType::Interface { description, .. }
                | MetaType::Union { description, .. }
                | MetaType::Enum { description, .. }
                | MetaType::InputObject { description, .. } => description.as_deref(),
            }),
            "fields" => meta
                .fields()
                .map(|fields| {
                    Datum::Many(
                        fields
                            .values()
                            .filter(|f| {
                                !f.name.starts_with("__")
                                    && (include || !f.deprecation.is_deprecated())
                            })
                            .map(Subject::Field)
                            .collect(),
                    )
                })
                .unwrap_or(Datum::Leaf(Value::Null)),
            "interfaces" => match meta {
                MetaType::Object { name, .. } => Datum::Many(
                    self.registry
                        .implements
                        .get(name)
                        .into_iter()
                        .flatten()
                        .map(|name| Subject::Type(name.clone()))
                        .collect(),
                ),
                _ => Datum::Leaf(Value::Null),
            },
            "possibleTypes" => match meta {
                MetaType::Interface { possible_types, .. }
                | MetaType::Union { possible_types, .. } => Datum::Many(
                    possible_types
                        .iter()
                        .map(|name| Subject::Type(name.clone()))
                        .collect(),
                ),
                _ => Datum::Leaf(Value::Null),
            },
            "enumValues" => match meta {
                MetaType::Enum { enum_values, .. } => Datum::Many(
                    enum_values
                        .values()
                        .filter(|v| include || !v.deprecation.is_deprecated())
                        .map(Subject::Enum)
                        .collect(),
                ),
                _ => Datum::Leaf(Value::Null),
            },
            "inputFields" => match meta {
                MetaType::InputObject { input_fields, .. } => Datum::Many(
                    input_fields
                        .values()
                        .filter(|v| include || !v.deprecation.is_deprecated())
                        .map(Subject::Input)
                        .collect(),
                ),
                _ => Datum::Leaf(Value::Null),
            },
            "specifiedByURL" => match meta {
                MetaType::Scalar {
                    specified_by_url, ..
                } => text(specified_by_url.as_deref()),
                _ => Datum::Leaf(Value::Null),
            },
            "isOneOf" => match meta {
                MetaType::InputObject { oneof, .. } => boolean(*oneof),
                _ => Datum::Leaf(Value::Null),
            },
            _ => Datum::Leaf(Value::Null),
        }
    }
}

fn merge(previous: &mut Value, next: Value) {
    match (previous, next) {
        (Value::Object(previous), Value::Object(next)) => {
            for (key, value) in next {
                if let Some(previous) = previous.get_mut(&key) {
                    merge(previous, value);
                } else {
                    previous.insert(key, value);
                }
            }
        }
        (Value::List(previous), Value::List(next)) => {
            for (previous, next) in previous.iter_mut().zip(next) {
                merge(previous, next);
            }
        }
        (previous, next) => *previous = next,
    }
}

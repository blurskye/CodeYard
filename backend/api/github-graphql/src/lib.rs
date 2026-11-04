//! GitHub's schema types, independent of the application and its storage.
//! Regenerate the type modules with `scripts/github-graphql-types.py`.
mod abstracts;
mod directives;
mod enums;
mod inputs;
mod introspection;
mod objects;
mod resolvers;
mod scalars;
mod schema;
mod schema_directive;
#[cfg(test)]
mod tests;

pub use abstracts::*;
pub use enums::*;
pub use inputs::*;
pub use objects::*;
pub use resolvers::*;
pub use scalars::*;

use async_graphql::{ErrorExtensions, InputType, Request, Response, Value};

pub type FieldFuture<'a, T> =
    async_graphql::futures_util::future::BoxFuture<'a, async_graphql::Result<T>>;

fn unsupported<'a, T: Send + 'a>(ty: &'static str, field: &'static str) -> FieldFuture<'a, T> {
    let error = async_graphql::Error::new(format!("{ty}.{field} is not implemented")).extend_with(
        |_, extensions| {
            extensions.set("code", "NOT_IMPLEMENTED");
        },
    );
    Box::pin(std::future::ready(Err(error)))
}

/// Whether a `Date` input is a date: `YYYY-MM-DD`, or an RFC 3339 time, as the resolvers read it.
/// Checking it while the request is parsed refuses it before any mutation of the request runs.
fn is_date(value: &Value) -> bool {
    matches!(value, Value::String(text)
        if chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok()
            || chrono::DateTime::parse_from_rfc3339(text).is_ok())
}

fn default_value<T: InputType>(value: Value) -> T {
    T::parse(Some(value))
        .unwrap_or_else(|_| panic!("invalid generated default for {}", T::type_name()))
}

/// A boxed input that preserves omitted fields in introspected default literals.
/// Derived input objects otherwise serialize `{}` as `{optionalField: null}`.
#[derive(Clone, Debug)]
pub struct Input<T> {
    value: Box<T>,
    literal: Value,
}

impl<T> Input<T> {
    pub fn into_inner(self) -> T {
        *self.value
    }
}

impl<T> std::ops::Deref for Input<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T: InputType> InputType for Input<T> {
    type RawValueType = T::RawValueType;

    fn type_name() -> std::borrow::Cow<'static, str> {
        T::type_name()
    }

    fn create_type_info(registry: &mut async_graphql::registry::Registry) -> String {
        T::create_type_info(registry)
    }

    fn parse(value: Option<Value>) -> async_graphql::InputValueResult<Self> {
        let parsed = T::parse(value.clone()).map_err(async_graphql::InputValueError::propagate)?;
        Ok(Self {
            value: Box::new(parsed),
            literal: value.unwrap_or(Value::Null),
        })
    }

    fn to_value(&self) -> Value {
        self.literal.clone()
    }

    fn as_raw_value(&self) -> Option<&Self::RawValueType> {
        self.value.as_raw_value()
    }
}

struct SharedExtension(std::sync::Arc<dyn async_graphql::extensions::ExtensionFactory>);

impl async_graphql::extensions::ExtensionFactory for SharedExtension {
    fn create(&self) -> std::sync::Arc<dyn async_graphql::extensions::Extension> {
        self.0.create()
    }
}

/// Keeps schema construction and execution monomorphization in this crate.
pub struct GitHubSchema(async_graphql::Schema<Query, Mutation, async_graphql::EmptySubscription>);

impl GitHubSchema {
    pub fn new(query: Query, mutation: Mutation) -> Self {
        Self::with_extensions(query, mutation, Vec::new())
    }

    pub fn with_extensions(
        query: Query,
        mutation: Mutation,
        extensions: Vec<std::sync::Arc<dyn async_graphql::extensions::ExtensionFactory>>,
    ) -> Self {
        // Unoptimized derive metadata for the full SDL exceeds a Tokio worker's
        // stack. Only one-time registration needs this stack, not query execution.
        std::thread::Builder::new()
            .name("github-graphql-schema".into())
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                let mut builder =
                    schema::build(query, mutation).extension(introspection::OrphanUnions);
                for directive in directives::definitions() {
                    builder = builder.directive(directive);
                }
                for extension in extensions {
                    builder = builder.extension(SharedExtension(extension));
                }
                Self(builder.finish())
            })
            .expect("start GitHub schema registration")
            .join()
            .unwrap_or_else(|error| std::panic::resume_unwind(error))
    }

    pub fn execute(
        &self,
        request: Request,
    ) -> async_graphql::futures_util::future::BoxFuture<'_, Response> {
        Box::pin(self.0.execute(request))
    }

    pub fn sdl(&self) -> String {
        schema_directive::restore_argument_descriptions(&self.0.sdl(), &directives::definitions())
    }
}

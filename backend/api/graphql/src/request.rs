//! Preserve JSON numbers when serde_json's arbitrary-precision feature is enabled.
use serde_json::Value;

fn value(value: Value) -> async_graphql::Value {
    match value {
        Value::Null => async_graphql::Value::Null,
        Value::Bool(value) => async_graphql::Value::Boolean(value),
        Value::Number(value) => async_graphql::Value::Number(value),
        Value::String(value) => async_graphql::Value::String(value),
        Value::Array(values) => {
            async_graphql::Value::List(values.into_iter().map(crate::request::value).collect())
        }
        Value::Object(values) => async_graphql::Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (async_graphql::Name::new(key), crate::request::value(value)))
                .collect(),
        ),
    }
}

pub(super) fn parse(mut body: Value) -> serde_json::Result<async_graphql::Request> {
    let variables = body
        .as_object_mut()
        .and_then(|body| body.remove("variables"));
    let mut request: async_graphql::Request = serde_json::from_value(body)?;
    if let Some(variables) = variables {
        // Retain upstream validation of the variables object's shape.
        let variables: serde_json::Map<String, Value> = if variables.is_null() {
            Default::default()
        } else {
            serde_json::from_value(variables)?
        };
        for (key, v) in variables {
            request
                .variables
                .insert(async_graphql::Name::new(key), value(v));
        }
    }
    Ok(request)
}

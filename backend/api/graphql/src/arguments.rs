//! Borrowed views over already validated, typed GraphQL arguments.
use crate::unavailable;
use crate::Result;
use async_graphql::indexmap::IndexMap;
use async_graphql::Error;
use async_graphql::Name;
use async_graphql::Value;
use serde::de::DeserializeOwned;

pub(super) struct Arguments {
    pub args: ArgumentMap,
    pub count_only: bool,
}

pub(super) struct ArgumentMap(IndexMap<Name, Value>);

#[derive(Clone, Copy)]
pub(super) struct ObjectAccessor<'a>(&'a IndexMap<Name, Value>);

#[derive(Clone, Copy)]
pub(super) struct ValueAccessor<'a>(&'a Value);

impl Arguments {
    pub fn new(values: impl IntoIterator<Item = (String, Value)>) -> Self {
        Self {
            count_only: false,
            args: ArgumentMap(
                values
                    .into_iter()
                    .map(|(name, value)| (Name::new(name), value))
                    .collect(),
            ),
        }
    }
}

impl Arguments {
    pub fn with_selection(mut self, ctx: &async_graphql::Context<'_>) -> Self {
        let selection = ctx.look_ahead();
        self.count_only = !selection.field("nodes").exists()
            && !selection.field("edges").exists()
            && !selection.field("pageInfo").exists();
        self
    }
}

impl ArgumentMap {
    pub fn get(&self, name: &str) -> Option<ValueAccessor<'_>> {
        self.0.get(name).map(ValueAccessor)
    }

    pub fn try_get(&self, name: &str) -> Result<ValueAccessor<'_>> {
        self.get(name)
            .ok_or_else(|| Error::new(format!("Missing {name}")))
    }
}

impl<'a> ObjectAccessor<'a> {
    pub fn try_get(&self, name: &str) -> Result<ValueAccessor<'a>> {
        self.0
            .get(name)
            .map(ValueAccessor)
            .ok_or_else(|| Error::new(format!("Missing {name}")))
    }
}

impl<'a> ValueAccessor<'a> {
    pub fn is_null(&self) -> bool {
        matches!(self.0, Value::Null)
    }

    pub fn string(&self) -> Result<&'a str> {
        match self.0 {
            Value::String(value) => Ok(value),
            _ => Err(Error::new("Expected a string")),
        }
    }

    pub fn enum_name(&self) -> Result<&'a str> {
        match self.0 {
            Value::Enum(value) => Ok(value.as_str()),
            Value::String(value) => Ok(value),
            _ => Err(Error::new("Expected an enum")),
        }
    }

    pub fn i64(&self) -> Result<i64> {
        match self.0 {
            Value::Number(value) => value
                .as_i64()
                .ok_or_else(|| Error::new("Expected an integer")),
            _ => Err(Error::new("Expected an integer")),
        }
    }

    pub fn object(&self) -> Result<ObjectAccessor<'a>> {
        match self.0 {
            Value::Object(value) => Ok(ObjectAccessor(value)),
            _ => Err(Error::new("Expected an object")),
        }
    }

    pub fn deserialize<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.0.clone().into_json().map_err(unavailable)?)
            .map_err(unavailable)
    }
}

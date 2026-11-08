use async_graphql::{Error, ErrorExtensions};
pub fn error(message: impl Into<String>, kind: &str) -> Error {
    Error::new(message).extend_with(|_, e| e.set("type", kind))
}

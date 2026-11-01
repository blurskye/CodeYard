//! A JSON request body that may be left out, for endpoints whose fields are all optional.
use super::json_parse_error;
use axum::{
    async_trait,
    body::Bytes,
    extract::{FromRequest, Request},
    response::{IntoResponse, Response},
};
use serde::de::DeserializeOwned;

/// `None` for an empty body or JSON `null`. Any other body must parse, whatever its content
/// type, as GitHub reads it: a malformed body is refused rather than read as no options.
pub(super) struct OptionalJson<T>(pub Option<T>);

pub(super) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> super::Result<Option<T>> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    serde_json::from_slice(bytes).map_err(json_parse_error)
}

#[async_trait]
impl<T, S> FromRequest<S> for OptionalJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(request, state)
            .await
            .map_err(IntoResponse::into_response)?;
        decode(&bytes)
            .map(Self)
            .map_err(IntoResponse::into_response)
    }
}

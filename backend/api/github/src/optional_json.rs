//! A JSON request body that may be left out, for endpoints whose fields are all optional.
use axum::async_trait;
use axum::body::Bytes;
use axum::extract::FromRequest;
use axum::extract::Request;
use axum::response::IntoResponse;
use axum::response::Response;
use codeyard_model::github_error::json_parse_error;
use serde::de::DeserializeOwned;

/// `None` for an empty body or JSON `null`. Any other body must parse, whatever its content
/// type, as GitHub reads it: a malformed body is refused rather than read as no options.
pub(super) struct OptionalJson<T>(pub Option<T>);

pub(super) fn decode<T: DeserializeOwned>(
    bytes: &[u8],
) -> codeyard_model::github_error::Result<Option<T>> {
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

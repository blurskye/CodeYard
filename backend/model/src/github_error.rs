use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
#[derive(Debug)]
pub struct Error(StatusCode, String, Option<Value>);

pub type Result<T> = std::result::Result<T, Error>;

/// Preserve Axum's transport/syntax/data classification inside the GitHub envelope.
pub fn json_error(error: axum::extract::rejection::JsonRejection) -> Error {
    Error::new(error.status(), error.body_text())
}

pub fn json_bytes_error(error: axum::Error) -> Error {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(error) = source {
        if error.is::<http_body_util::LengthLimitError>() {
            return Error::new(StatusCode::PAYLOAD_TOO_LARGE, "Request body is too large");
        }
        source = error.source();
    }
    Error::new(StatusCode::BAD_REQUEST, "Failed to read request body")
}

/// Byte-buffered endpoints intentionally keep their optional-body and media-type policies.
pub fn json_parse_error(error: serde_json::Error) -> Error {
    let status = if error.is_data() {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::BAD_REQUEST
    };
    Error::new(status, error.to_string())
}

impl Error {
    pub fn graphql(self) -> async_graphql::Error {
        let kind = match self.0 {
            StatusCode::NOT_FOUND => "NOT_FOUND",
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => "FORBIDDEN",
            StatusCode::SERVICE_UNAVAILABLE => "INTERNAL",
            _ => "UNPROCESSABLE",
        };
        crate::graphql_error::error(self.1, kind)
    }

    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self(status, message.into(), None)
    }

    pub fn missing() -> Self {
        Self::new(StatusCode::NOT_FOUND, "Not Found")
    }

    pub fn unavailable<E: std::fmt::Debug>(error: E) -> Self {
        tracing::warn!(?error, "GitHub API dependency unavailable");
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "Service unavailable")
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, message)
    }

    pub fn invalid_simple(message: impl Into<String>) -> Self {
        Self::invalid(message).simple()
    }

    /// Lists a 422's message as a plain string, for operations GitHub documents with
    /// `validation-error-simple`.
    pub fn simple(mut self) -> Self {
        if self.0 == StatusCode::UNPROCESSABLE_ENTITY && self.2.is_none() {
            self.2 = Some(json!([self.1]));
        }
        self
    }

    pub fn validation(resource: &str, field: &str, code: &str) -> Self {
        Self(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Validation Failed".into(),
            Some(json!([{"resource": resource, "field": field, "code": code}])),
        )
    }

    pub fn response_with_docs(self, docs: &str) -> Response {
        let mut body = json!({
            "message": self.1,
            "documentation_url": docs,
            "status": self.0.as_u16().to_string(),
        });
        if let Some(errors) = self.2 {
            body["errors"] = errors;
        } else if self.0 == StatusCode::UNPROCESSABLE_ENTITY {
            body["errors"] = json!([{"code": "custom", "message": self.1}]);
        }
        if self.0 == StatusCode::CONFLICT {
            body["metadata"] = Value::Null;
        }
        (self.0, Json(body)).into_response()
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        self.response_with_docs("https://docs.github.com/en/rest")
    }
}

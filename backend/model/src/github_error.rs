use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
#[derive(Debug)]
pub struct Error(pub StatusCode, pub String, pub Option<Value>);

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
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Service unavailable. Check the result before retrying the request.",
        )
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
        let mut response = (self.0, Json(body)).into_response();
        // A frozen account is refused by the writing transaction, like other temporary
        // dependency failures. Give clients a bounded retry hint without exposing SQL
        // diagnostics or claiming that an uncertain write was rolled back.
        if self.0 == StatusCode::SERVICE_UNAVAILABLE {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("5"),
            );
        }
        response
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        self.response_with_docs("https://docs.github.com/en/rest")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unavailable_responses_give_retry_guidance_without_driver_details() {
        let response = Error::unavailable("private database diagnostic").into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[axum::http::header::RETRY_AFTER], "5");
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], "503");
        assert_eq!(
            body["message"],
            "Service unavailable. Check the result before retrying the request."
        );
        assert!(!String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("private"));
    }

    #[test]
    fn retry_hint_is_limited_to_temporary_unavailability() {
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::CONFLICT,
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let response = Error::new(status, "refused").into_response();
            assert_eq!(response.status(), status);
            assert!(!response
                .headers()
                .contains_key(axum::http::header::RETRY_AFTER));
        }
        let response = Error::new(StatusCode::SERVICE_UNAVAILABLE, "temporarily unavailable")
            .response_with_docs("https://docs.github.com/en/rest/issues/issues");
        assert_eq!(response.headers()[axum::http::header::RETRY_AFTER], "5");
    }
}

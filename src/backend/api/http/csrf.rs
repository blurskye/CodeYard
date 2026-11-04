//! Use the DB/session-bound synchronizer and origin policy of native forms.

use crate::backend::api::http::context::ExistingCsrfError;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::error::ApiFailure;

use crate::backend::app::AppState;
use axum::{
    extract::Request,
    http::{HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension,
};
use std::sync::Arc;

pub(crate) async fn check(state: Arc<AppState>, headers: &HeaderMap) -> Result<(), ApiFailure> {
    let mut supplied = headers.get_all("x-csrf-token").iter();
    let token = supplied
        .next()
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() == 64)
        .ok_or_else(denied)?;
    if supplied.next().is_some()
        || !crate::backend::api::http::origin::same_origin_submission(headers)
    {
        return Err(denied());
    }
    let expected =
        crate::backend::api::http::context::csrf_token_for_existing_session(&state, headers)
            .await
            .map_err(|error| match error {
                ExistingCsrfError::Missing => denied(),
                ExistingCsrfError::Unavailable => ApiError::failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "csrf_unavailable",
                    "Session protection unavailable",
                ),
            })?;
    if crate::backend::api::http::context::csrf_matches(&expected, Some(token)) {
        Ok(())
    } else {
        Err(denied())
    }
}
pub(crate) async fn cookie_api_submission(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) && crate::backend::api::http::session::has_session_cookie(request.headers())
    {
        if let Err(error) = check(state, request.headers()).await {
            return error.into_response();
        }
    }
    next.run(request).await
}

fn denied() -> ApiFailure {
    ApiError::failure(
        StatusCode::FORBIDDEN,
        "csrf_error",
        "Reload the page and supply its session-bound CSRF token",
    )
}

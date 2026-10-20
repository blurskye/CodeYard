//! Use the DB/session-bound synchronizer and origin policy of native forms.
use super::{
    context::{self, ExistingCsrfError},
    error::ApiError,
    origin, session,
};
use crate::backend::app::AppState;
use axum::{
    extract::Request,
    http::{header, HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension, Json,
};
use std::sync::Arc;

type Failure = (StatusCode, Json<ApiError>);

fn failure(status: StatusCode, code: &str, message: &str) -> Failure {
    (status, Json(ApiError::new(code, message)))
}

pub(crate) async fn check(state: Arc<AppState>, headers: &HeaderMap) -> Result<(), Failure> {
    let mut supplied = headers.get_all("x-csrf-token").iter();
    let token = supplied
        .next()
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() == 64)
        .ok_or_else(denied)?;
    if supplied.next().is_some() || !origin::same_origin_submission(headers) {
        return Err(denied());
    }
    let expected = context::csrf_token_for_existing_session(&state, headers)
        .await
        .map_err(|error| match error {
            ExistingCsrfError::Missing => denied(),
            ExistingCsrfError::Unavailable => failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "csrf_unavailable",
                "Session protection unavailable",
            ),
        })?;
    if context::csrf_matches(&expected, Some(token)) {
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
    ) && session::has_session_cookie(request.headers())
    {
        if let Err(error) = check(state, request.headers()).await {
            return error.into_response();
        }
    }
    next.run(request).await
}

pub(crate) async fn auth_submission(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        if !origin::same_origin_submission(request.headers()) {
            return denied().into_response();
        }
        // First login/setup/session creation has no synchronizer. Requests
        // carrying an existing browser session must prove that session even
        // when the endpoint will rotate or clear its cookie.
        if session::has_session_cookie(request.headers()) {
            if let Err(error) = check(state.clone(), request.headers()).await {
                let mut response = error.into_response();
                if let Some(id) = session::extract_session_id(request.headers()) {
                    if matches!(
                        crate::backend::db::users::sessions::get_browser_session(&state.pool, &id)
                            .await,
                        Ok(None)
                    ) {
                        if let Ok(cookie) = session::build_logout_cookie().parse() {
                            response.headers_mut().append(header::SET_COOKIE, cookie);
                        }
                    }
                }
                return response;
            }
        } else if request.uri().path().ends_with("/logout") {
            // A cross-site form without a Lax cookie must not clear one.
            return denied().into_response();
        }
    }
    next.run(request).await
}

fn denied() -> Failure {
    failure(
        StatusCode::FORBIDDEN,
        "csrf_error",
        "Reload the page and supply its session-bound CSRF token",
    )
}

use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use codeyard_app::app::AppState;
use codeyard_git::gist_types::MAX_GIST_REQUEST_BYTES;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use std::sync::Arc;
static GIST_UPLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

pub async fn buffered_write(
    axum::Extension(state): axum::Extension<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Err(error) = writer(&state, request.headers()).await {
        return error.into_response();
    }
    let Ok(_permit) = GIST_UPLOADS.try_acquire() else {
        return Error::new(StatusCode::SERVICE_UNAVAILABLE, "Gist uploads are busy")
            .into_response();
    };
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        axum::body::to_bytes(body, MAX_GIST_REQUEST_BYTES),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return Error::new(StatusCode::PAYLOAD_TOO_LARGE, "Gist request is too large")
                .into_response()
        }
        Err(_) => {
            return Error::new(StatusCode::REQUEST_TIMEOUT, "Gist request timed out")
                .into_response()
        }
    };
    next.run(axum::extract::Request::from_parts(
        parts,
        axum::body::Body::from(bytes),
    ))
    .await
}

pub async fn writer(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<codeyard_repos_core::api_shared::github::auth::Authentication> {
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    actor.require_write()?;
    Ok(actor)
}

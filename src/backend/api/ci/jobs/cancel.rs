use super::{parse_job_id, CiReadError};
use crate::backend::{
    api::{http::error::ApiError, http::session::get_current_user},
    app::AppState,
};
use axum::{
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct CancelCiRequest {
    pub csrf_token: String,
}

#[derive(Debug, Serialize)]
pub struct CancelCiResponse {
    pub requested: bool,
}

pub async fn cancel_ci_job(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(job_id): Path<String>,
    Json(request): Json<CancelCiRequest>,
) -> Result<Json<CancelCiResponse>, CiReadError> {
    // Current identity-bound, same-origin CSRF; never a GET mutation.
    let context =
        crate::backend::api::http::context::PageContext::load(state.clone(), headers.clone(), &uri)
            .await
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ApiError::new(
                        "csrf_unavailable",
                        "Current session authority is unavailable",
                    )),
                )
            })?;
    let form = std::collections::HashMap::from([("csrf_token".to_owned(), request.csrf_token)]);
    context.check_csrf(&form).map_err(|_| {
        (
            StatusCode::FORBIDDEN,
            Json(ApiError::new(
                "csrf_error",
                "A current same-origin CSRF token is required",
            )),
        )
    })?;
    let user = get_current_user(&state.pool, &headers).await?;
    let requested = state
        .ci_runs
        .cancel_run(parse_job_id(&job_id)?, user.id)
        .await
        .map_err(super::super::admission_error)?;
    Ok(Json(CancelCiResponse { requested }))
}

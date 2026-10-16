use crate::backend::{
    api::session::get_current_user,
    app::AppState,
    files::{self, ConfirmationError},
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use super::{failure, Failure};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmUploadRequest {
    pub file_id: Uuid,
}
#[derive(Serialize)]
pub struct ConfirmUploadResponse {
    pub success: bool,
}

pub async fn confirm_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ConfirmUploadRequest>,
) -> Result<Json<ConfirmUploadResponse>, Failure> {
    let user = get_current_user(&state.pool, &headers).await?;
    super::super::csrf::check(state.clone(), &headers).await?;
    files::confirm_upload(&state.pool, &state.storage, user.id, req.file_id)
        .await
        .map_err(confirmation_failure)?;
    Ok(Json(ConfirmUploadResponse { success: true }))
}

fn confirmation_failure(error: ConfirmationError) -> Failure {
    let (status, code, message) = match error {
        ConfirmationError::MetadataUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "Upload metadata is unavailable",
        ),
        ConfirmationError::NotFound => (StatusCode::NOT_FOUND, "not_found", "Upload not found"),
        ConfirmationError::NotPending => (
            StatusCode::CONFLICT,
            "not_pending",
            "Upload is expired or already confirmed",
        ),
        ConfirmationError::StorageUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "Upload storage is unavailable; retry this pending upload",
        ),
        ConfirmationError::VerificationFailed => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "verification_failed",
            "The uploaded file is missing or does not match its declaration",
        ),
    };
    failure(status, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_errors_keep_wire_status_codes_and_messages() {
        let cases = [
            (
                ConfirmationError::MetadataUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "database_unavailable",
                "Upload metadata is unavailable",
            ),
            (
                ConfirmationError::NotFound,
                StatusCode::NOT_FOUND,
                "not_found",
                "Upload not found",
            ),
            (
                ConfirmationError::NotPending,
                StatusCode::CONFLICT,
                "not_pending",
                "Upload is expired or already confirmed",
            ),
            (
                ConfirmationError::StorageUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
                "Upload storage is unavailable; retry this pending upload",
            ),
            (
                ConfirmationError::VerificationFailed,
                StatusCode::UNPROCESSABLE_ENTITY,
                "verification_failed",
                "The uploaded file is missing or does not match its declaration",
            ),
        ];
        for (outcome, expected_status, code, message) in cases {
            let (status, Json(error)) = confirmation_failure(outcome);
            assert_eq!(status, expected_status);
            assert_eq!(error.error, code);
            assert_eq!(error.message, message);
        }
    }
}

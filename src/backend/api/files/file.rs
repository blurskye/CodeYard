use crate::backend::{
    api::http::session::get_current_user,
    app::AppState,
    files::{self, DeleteError},
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use std::sync::Arc;
use uuid::Uuid;

use super::{failure, Failure};

pub async fn delete_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(file_id): Path<String>,
) -> Result<StatusCode, Failure> {
    let user = get_current_user(&state.pool, &headers).await?;
    super::super::http::csrf::check(state.clone(), &headers).await?;
    let file_uuid = Uuid::parse_str(&file_id).map_err(|_| {
        failure(
            StatusCode::BAD_REQUEST,
            "invalid_file_id",
            "Invalid file ID format",
        )
    })?;
    files::delete_owned_file(&state.pool, &state.storage, user.id, file_uuid)
        .await
        .map_err(deletion_failure)?;
    Ok(StatusCode::NO_CONTENT)
}

fn deletion_failure(error: DeleteError) -> Failure {
    let (status, code, message) = match error {
        DeleteError::MetadataUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "File metadata is unavailable",
        ),
        DeleteError::StorageUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "File storage is unavailable; the file was not deleted. Try again.",
        ),
        DeleteError::NotFound => (StatusCode::NOT_FOUND, "not_found", "File not found"),
        DeleteError::Denied => (
            StatusCode::FORBIDDEN,
            "denied",
            "File deletion is not allowed",
        ),
        DeleteError::ReferencedOrganizationAvatar => (
            StatusCode::CONFLICT,
            "file_in_use",
            "This file is the current organization picture",
        ),
        DeleteError::ReferencedReleaseAsset => (
            StatusCode::CONFLICT,
            "file_in_use",
            "This file is a release asset. Delete it from its release.",
        ),
    };
    failure(status, code, message)
}

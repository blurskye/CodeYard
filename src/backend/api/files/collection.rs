use crate::backend::{
    api::session::get_current_user,
    app::AppState,
    db::models::{FileReason, FileVisibility},
    files::{self, UploadCapability, UploadDeclaration, UploadError},
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

use super::{failure, Failure};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateUploadUrlRequest {
    pub filename: String,
    pub content_type: String,
    pub file_size: i64,
    #[serde(deserialize_with = "deserialize_reason")]
    pub reason: FileReason,
    pub visibility: FileVisibility,
    /// Required lowercase hex SHA256 of the exact bytes to upload.
    pub sha256_hex: String,
}

fn deserialize_reason<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<FileReason, D::Error> {
    // Keep established wire names (not FileReason's derived variant spelling
    // for PullRequestAttachment) while decoding to an explicit typed intent.
    String::deserialize(deserializer)?
        .parse()
        .map_err(|_| serde::de::Error::custom("Unsupported upload intent"))
}

#[derive(Serialize)]
pub struct CreateUploadUrlResponse {
    pub upload_url: String,
    pub headers: BTreeMap<String, String>,
    pub file_id: String,
    /// Object key in the bucket, for direct S3 uploads.
    pub s3_key: Option<String>,
    pub expires_in: u64,
    pub capability: UploadCapability,
    pub method: &'static str,
    pub csrf_required: bool,
}

pub async fn create_upload_url(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateUploadUrlRequest>,
) -> Result<Json<CreateUploadUrlResponse>, Failure> {
    let user = get_current_user(&state.pool, &headers).await?;
    super::super::csrf::check(state.clone(), &headers).await?;
    let reservation = files::reserve_and_sign_upload(
        &state.pool,
        &state.storage,
        user.id,
        UploadDeclaration {
            filename: &req.filename,
            content_type: &req.content_type,
            file_size: req.file_size,
            reason: req.reason,
            visibility: req.visibility,
            sha256_hex: &req.sha256_hex,
        },
    )
    .await
    .map_err(upload_failure)?;
    Ok(Json(CreateUploadUrlResponse {
        upload_url: reservation.url,
        headers: reservation.headers,
        file_id: reservation.file_id.to_string(),
        s3_key: (reservation.capability == UploadCapability::S3DirectPut)
            .then_some(reservation.object_key),
        expires_in: reservation.expires_in,
        capability: reservation.capability,
        method: reservation.method,
        csrf_required: reservation.capability == UploadCapability::AuthenticatedLocalPut,
    }))
}

fn upload_failure(error: UploadError) -> Failure {
    let (status, code, message) = match error {
        UploadError::Invalid => (
            StatusCode::BAD_REQUEST,
            "invalid_upload",
            "Invalid upload declaration or visibility",
        ),
        UploadError::Quota => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Upload quota exhausted",
        ),
        UploadError::AdmissionUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "upload_unavailable",
            "Upload admission is unavailable",
        ),
        UploadError::SigningUnavailable { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "signing_failed",
            "Upload signing is unavailable",
        ),
        UploadError::CleanupFailed { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "upload_cleanup_incomplete",
            "Upload failed and its file could not be fully removed",
        ),
        UploadError::StorageUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "Upload storage is unavailable",
        ),
    };
    failure(status, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_errors_keep_wire_status_codes_and_messages() {
        let cases = [
            (
                UploadError::Invalid,
                StatusCode::BAD_REQUEST,
                "invalid_upload",
                "Invalid upload declaration or visibility",
            ),
            (
                UploadError::Quota,
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "Upload quota exhausted",
            ),
            (
                UploadError::AdmissionUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "upload_unavailable",
                "Upload admission is unavailable",
            ),
            (
                UploadError::SigningUnavailable {
                    file_id: uuid::Uuid::nil(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
                "signing_failed",
                "Upload signing is unavailable",
            ),
        ];
        for (outcome, expected_status, code, message) in cases {
            let (status, Json(error)) = upload_failure(outcome);
            assert_eq!(status, expected_status);
            assert_eq!(error.error, code);
            assert_eq!(error.message, message);
        }
    }

    #[test]
    fn typed_intent_keeps_wire_names_and_requires_explicit_visibility_checksum() {
        let value = serde_json::json!({"filename":"owned.txt", "content_type":"text/plain", "file_size":1,
            "reason":"pr_attachment", "visibility":"owner", "sha256_hex":"00".repeat(32)});
        let decoded: CreateUploadUrlRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(decoded.reason, FileReason::PullRequestAttachment);
        for key in ["visibility", "sha256_hex"] {
            let mut incomplete = value.clone();
            incomplete.as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<CreateUploadUrlRequest>(incomplete).is_err());
        }
        let mut invalid = value;
        invalid["reason"] = serde_json::json!("guessed_private_attachment");
        assert!(serde_json::from_value::<CreateUploadUrlRequest>(invalid).is_err());
    }
}

use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::error::ApiFailure;
use codeyard_api_http::session::extract_session_id;
use codeyard_api_http::session::get_current_user;
use codeyard_app::app::AppState;
use codeyard_storage::files::downloads::DownloadCapability;
use codeyard_storage::files::downloads::DownloadError;
use codeyard_storage::files::downloads::SignedDownload;
use serde::Serialize;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Serialize)]
pub struct DownloadUrlResponse {
    pub download_url: String,
    pub filename: String,
    pub content_type: String,
    pub file_size: i64,
    pub expires_in: u64,
    pub capability: DownloadCapability,
}

pub(super) fn parse_file_id(id: &str) -> Result<Uuid, ApiFailure> {
    Uuid::parse_str(id).map_err(|_| {
        ApiError::failure(
            StatusCode::BAD_REQUEST,
            "invalid_file_id",
            "Invalid file ID",
        )
    })
}

pub(super) fn download_response(download: SignedDownload) -> Response {
    private_response(Json(DownloadUrlResponse {
        download_url: download.url,
        filename: download.filename,
        content_type: download.content_type,
        file_size: download.file_size,
        expires_in: download.expires_in,
        capability: download.capability,
    }))
}

pub async fn get_download_url(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(file_id): Path<String>,
) -> Result<Response, ApiFailure> {
    let user = get_current_user(&state.pool, &headers).await?;
    let file_id = parse_file_id(&file_id)?;
    let download = codeyard_storage::files::downloads::download_for_user(
        &state.pool,
        &state.storage,
        (user.id, extract_session_id(&headers).unwrap_or_default()),
        file_id,
    )
    .await
    .map_err(download_failure)?;
    Ok(download_response(download))
}

pub(super) fn download_failure(error: DownloadError) -> ApiFailure {
    let (status, code, message) = match error {
        DownloadError::MetadataUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "File metadata is unavailable",
        ),
        DownloadError::NotFound => (
            StatusCode::NOT_FOUND,
            "not_found",
            "File not found or not accessible",
        ),
        DownloadError::StorageUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "s3_unavailable",
            "Object storage is unavailable",
        ),
        DownloadError::SigningUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "signing_unavailable",
            "Download signing is unavailable",
        ),
        DownloadError::AccountingUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "accounting_unavailable",
            "Download accounting is unavailable",
        ),
    };
    ApiError::failure(status, code, message)
}

use codeyard_api_http::shared::files::download::private_response;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;
    use axum::http::Method;
    use axum::response::Redirect;
    use codeyard_api_http::shared::files::download::asset_response;
    use codeyard_api_http::shared::files::download::download_read;
    use codeyard_storage::attachment_disposition;
    use codeyard_storage::files::downloads::AssetDownload;
    use codeyard_storage::files::downloads::DownloadTransport;
    use std::time::Duration;

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropped_read_keeps_its_io_slot_until_the_read_returns() {
        // A pipe gives a read that blocks until the writer closes.
        let (reader, writer) = std::io::pipe().unwrap();
        let reader = std::fs::File::from(std::os::fd::OwnedFd::from(reader));
        let capacity = Arc::new(tokio::sync::Semaphore::new(1));
        let io_guard = Arc::new(capacity.clone().acquire_owned().await.unwrap());
        drop(download_read(reader, 1, io_guard)); // the client went away
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(capacity.available_permits(), 0);
        drop(writer);
        let _released = tokio::time::timeout(Duration::from_secs(3), capacity.acquire())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn head_download_returns_metadata_not_a_get_signed_redirect() {
        let response = asset_response(
            AssetDownload {
                transport: DownloadTransport::RemoteUrl("https://objects.test/get-signed".into()),
                inline_type: None,
                filename: "demo.txt".into(),
                file_size: 42,
            },
            Method::HEAD,
        )
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "42");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        assert!(axum::body::to_bytes(response.into_body(), 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn attachment_names_never_become_paths_or_raw_headers() {
        assert_eq!(
            attachment_disposition("../../owned.txt"),
            "attachment; filename=\"owned.txt\"; filename*=UTF-8''owned.txt"
        );
        assert_eq!(
            attachment_disposition(".."),
            "attachment; filename=\"download\"; filename*=UTF-8''download"
        );
        let header = attachment_disposition("bad\r\n\"é.txt");
        assert!(!header.contains('\r') && !header.contains('\n'));
        assert!(axum::http::HeaderValue::from_str(&header).is_ok());
        assert!(header.contains("%22%C3%A9.txt"));
    }

    #[test]
    fn signed_capabilities_and_redirects_keep_private_security_headers() {
        for response in [
            download_response(SignedDownload {
                url: "https://objects.example.test/signed".into(),
                filename: "owned.txt".into(),
                content_type: "text/plain".into(),
                file_size: 7,
                expires_in: 300,
                capability: DownloadCapability::S3PresignedGet,
            }),
            private_response(Redirect::temporary("https://objects.example.test/signed")),
        ] {
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "no-store, private"
            );
            assert_eq!(response.headers()[header::VARY], "Cookie, Authorization");
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
        }
    }

    #[test]
    fn invalid_id_and_operation_errors_keep_wire_status_codes_and_messages() {
        let (status, Json(error)) = parse_file_id("not-a-uuid").unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error.error, "invalid_file_id");
        assert_eq!(error.message, "Invalid file ID");
        let cases = [
            (
                DownloadError::MetadataUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "database_unavailable",
                "File metadata is unavailable",
            ),
            (
                DownloadError::NotFound,
                StatusCode::NOT_FOUND,
                "not_found",
                "File not found or not accessible",
            ),
            (
                DownloadError::StorageUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "s3_unavailable",
                "Object storage is unavailable",
            ),
            (
                DownloadError::SigningUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "signing_unavailable",
                "Download signing is unavailable",
            ),
            (
                DownloadError::AccountingUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "accounting_unavailable",
                "Download accounting is unavailable",
            ),
        ];
        for (outcome, expected_status, code, message) in cases {
            let (status, Json(error)) = download_failure(outcome);
            assert_eq!(status, expected_status);
            assert_eq!(error.error, code);
            assert_eq!(error.message, message);
        }
    }
}

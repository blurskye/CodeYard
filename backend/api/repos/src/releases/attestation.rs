use axum::extract::Path;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_identity::repository_access::AccessError;
use codeyard_model::release_integrity::Attestation;
use codeyard_repos_core::repositories::releases::ReleaseError;
use std::sync::Arc;

pub(super) async fn download(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id)): Path<(String, String, i64)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    let found = match codeyard_repos_pulls::repositories::releases::detail_by_id(
        &ctx.state.pool,
        ctx.user.as_ref().map(|user| user.id),
        loaded.repository.id,
        id,
    )
    .await
    {
        Ok(found) => found,
        Err(ReleaseError::NotFound | ReleaseError::Access(AccessError::NotFound)) => {
            return ctx.error(StatusCode::NOT_FOUND, "Release not found.")
        }
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Release storage unavailable.",
            )
        }
    };
    if !found.release.release.is_immutable {
        return ctx.error(StatusCode::NOT_FOUND, "Release attestation not found.");
    }
    let (key, body) = match codeyard_db_repos::repositories::immutable_releases::attestation(
        &ctx.state.pool,
        found.repository.id,
        id,
    )
    .await
    {
        Ok(Some(record)) => record,
        _ => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Release attestation unavailable.",
            )
        }
    };
    let valid = serde_json::from_str::<Attestation>(&body)
        .ok()
        .is_some_and(|attestation| attestation.verify(&key).is_ok());
    if !valid {
        return ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Release attestation failed integrity verification.",
        );
    }
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"release-attestation.json\""),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

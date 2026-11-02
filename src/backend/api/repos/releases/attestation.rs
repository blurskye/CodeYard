use crate::backend::{
    api::http::context::PageContext,
    app::AppState,
    db,
    repositories::{
        access::AccessError,
        releases::{self, integrity::Attestation, ReleaseError},
    },
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
};
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
    let found = match releases::detail_by_id(
        &ctx.state.pool,
        ctx.user.as_ref().map(|user| user.id),
        &owner,
        &name,
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
    let (key, body) = match db::repositories::immutable_releases::attestation(
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

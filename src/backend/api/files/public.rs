use crate::backend::api::http::session::extract_session_id;
use crate::backend::api::http::session::get_optional_user;
use crate::backend::app::AppState;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, Method},
    response::Response,
};
use std::sync::Arc;

use crate::backend::api::files::download::asset_response;
use crate::backend::api::files::download::download_failure;
use crate::backend::api::files::download::download_response;
use crate::backend::api::files::download::parse_file_id;
use crate::backend::api::http::error::ApiFailure;

pub async fn get_public_download_url(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(file_id): Path<String>,
) -> Result<Response, ApiFailure> {
    let actor = get_optional_user(&state.pool, &headers)
        .await?
        .map(|user| user.id);
    let file_id = parse_file_id(&file_id)?;
    let download = crate::backend::files::download_asset(
        &state.pool,
        &state.storage,
        (actor, extract_session_id(&headers)),
        file_id,
    )
    .await
    .map_err(download_failure)?;
    Ok(download_response(download))
}

pub async fn public_file_redirect(
    State(state): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    Path(file_id): Path<String>,
) -> Result<Response, ApiFailure> {
    let actor = get_optional_user(&state.pool, &headers)
        .await?
        .map(|user| user.id);
    let file_id = parse_file_id(&file_id)?;
    let download = crate::backend::files::open_asset_download(
        &state.pool,
        &state.storage,
        (actor, extract_session_id(&headers)),
        file_id,
    )
    .await
    .map_err(download_failure)?;
    asset_response(download, method)
}

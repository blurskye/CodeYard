use crate::files::download::download_failure;
use crate::files::download::download_response;
use crate::files::download::parse_file_id;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::Method;
use axum::response::Response;
use codeyard_api_http::error::ApiFailure;
use codeyard_api_http::session::extract_session_id;
use codeyard_api_http::session::get_optional_user;
use codeyard_api_http::shared::files::download::asset_response;
use codeyard_app::app::AppState;
use std::sync::Arc;

pub async fn get_public_download_url(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(file_id): Path<String>,
) -> Result<Response, ApiFailure> {
    let actor = get_optional_user(&state.pool, &headers)
        .await?
        .map(|user| user.id);
    let file_id = parse_file_id(&file_id)?;
    let download = codeyard_storage::files::downloads::download_asset(
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
    let download = codeyard_storage::files::downloads::open_asset_download(
        &state.pool,
        &state.storage,
        (actor, extract_session_id(&headers)),
        file_id,
    )
    .await
    .map_err(download_failure)?;
    asset_response(download, method)
}

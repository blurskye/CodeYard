//! The release JSON API, shaped like GitHub's `/repos/{owner}/{repo}/releases`. Writers create,
//! change and delete releases; drafts are shown only to writers.
use super::pages::refused;
use crate::backend::api::http::config::public_origin;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::{get_current_user, get_optional_user};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::models::{ReleaseAsset, ReleaseRow};
use crate::backend::repositories::releases::{
    self as releases, ReleaseError, ReleaseFields, ReleaseInput,
};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

type Failure = (StatusCode, Json<ApiError>);

#[derive(Serialize)]
pub struct AssetResponse {
    id: i64,
    name: String,
    label: Option<String>,
    content_type: String,
    size: i64,
    download_count: i32,
    browser_download_url: String,
    created_at: String,
    updated_at: String,
}

#[derive(Serialize)]
pub struct Author {
    login: String,
}

#[derive(Serialize)]
pub struct ReleaseResponse {
    id: i64,
    tag_name: String,
    target_commitish: String,
    name: Option<String>,
    body: Option<String>,
    draft: bool,
    prerelease: bool,
    latest: bool,
    author: Author,
    created_at: String,
    published_at: Option<String>,
    html_url: String,
    tarball_url: String,
    zipball_url: String,
    assets: Vec<AssetResponse>,
}

#[derive(Deserialize)]
pub struct PageQuery {
    page: Option<i64>,
    per_page: Option<i64>,
}

/// The body of a create or update. An update changes only the fields it sends.
#[derive(Deserialize)]
pub struct ReleaseRequest {
    tag_name: Option<String>,
    target_commitish: Option<String>,
    name: Option<String>,
    body: Option<String>,
    draft: Option<bool>,
    prerelease: Option<bool>,
    make_latest: Option<bool>,
}

fn json_error(error: ReleaseError) -> Failure {
    let (status, message) = refused(&error);
    let code = match status {
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::UNPROCESSABLE_ENTITY => "validation_error",
        _ => "unavailable",
    };
    (status, Json(ApiError::new(code, &message)))
}

fn asset_response(origin: &str, base: &str, tag: &str, asset: &ReleaseAsset) -> AssetResponse {
    AssetResponse {
        id: asset.id,
        name: asset.name.clone(),
        label: asset.label.clone(),
        content_type: asset.content_type.clone(),
        size: asset.size,
        download_count: asset.download_count,
        browser_download_url: format!(
            "{origin}{base}/releases/download/{}/{}",
            crate::webui::encode_component(tag),
            crate::webui::encode_component(&asset.name)
        ),
        created_at: asset.created_at.to_rfc3339(),
        updated_at: asset.updated_at.to_rfc3339(),
    }
}

fn release_response(
    origin: &str,
    (owner, name): (&str, &str),
    row: &ReleaseRow,
    assets: &[ReleaseAsset],
) -> ReleaseResponse {
    let release = &row.release;
    let base = format!(
        "/{}/{}",
        crate::webui::encode_component(owner),
        crate::webui::encode_component(name)
    );
    let tag = crate::webui::encode_component(&release.tag_name);
    ReleaseResponse {
        id: release.id,
        tag_name: release.tag_name.clone(),
        target_commitish: release.target_commitish.clone(),
        name: release.name.clone(),
        body: release.body.clone(),
        draft: release.is_draft,
        prerelease: release.is_prerelease,
        latest: release.is_latest,
        author: Author {
            login: row.author.clone(),
        },
        created_at: release.created_at.to_rfc3339(),
        published_at: release.published_at.map(|date| date.to_rfc3339()),
        html_url: format!("{origin}{base}/releases/tag/{tag}"),
        tarball_url: format!("{origin}{base}/archive/{tag}.tar.gz"),
        zipball_url: format!("{origin}{base}/archive/{tag}.zip"),
        assets: assets
            .iter()
            .map(|asset| asset_response(origin, &base, &release.tag_name, asset))
            .collect(),
    }
}

const DEFAULT_PER_PAGE: i64 = 30;
const MAX_PER_PAGE: i64 = 100;

async fn release_with_assets(
    state: &AppState,
    row: &ReleaseRow,
) -> Result<Vec<ReleaseAsset>, Failure> {
    db::releases::list_release_assets(&state.pool, row.release.id)
        .await
        .map_err(|_| json_error(ReleaseError::Unavailable))
}

pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<PageQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<ReleaseResponse>>, Failure> {
    let viewer = get_optional_user(&state.pool, &headers)
        .await?
        .map(|user| user.id);
    let page = query.page.unwrap_or(1).max(1);
    let per_page = query
        .per_page
        .unwrap_or(DEFAULT_PER_PAGE)
        .clamp(1, MAX_PER_PAGE);
    let listing = releases::list(&state.pool, viewer, &owner, &name, page, per_page)
        .await
        .map_err(json_error)?;
    let origin = public_origin();
    let mut responses = Vec::with_capacity(listing.releases.len());
    for row in &listing.releases {
        let assets = release_with_assets(&state, row).await?;
        responses.push(release_response(&origin, (&owner, &name), row, &assets));
    }
    Ok(Json(responses))
}

pub(super) async fn get_one(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id)): Path<(String, String, i64)>,
    headers: HeaderMap,
) -> Result<Json<ReleaseResponse>, Failure> {
    let viewer = get_optional_user(&state.pool, &headers)
        .await?
        .map(|user| user.id);
    let found = releases::detail_by_id(&state.pool, viewer, &owner, &name, id)
        .await
        .map_err(json_error)?;
    Ok(Json(release_response(
        &public_origin(),
        (&owner, &name),
        &found.release,
        &found.assets,
    )))
}

pub(super) async fn get_by_tag(
    State(state): State<Arc<AppState>>,
    Path((owner, name, tag)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<ReleaseResponse>, Failure> {
    let viewer = get_optional_user(&state.pool, &headers)
        .await?
        .map(|user| user.id);
    let found = releases::detail_by_tag(&state.pool, viewer, &owner, &name, &tag)
        .await
        .map_err(json_error)?;
    Ok(Json(release_response(
        &public_origin(),
        (&owner, &name),
        &found.release,
        &found.assets,
    )))
}

pub(super) async fn create(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<ReleaseRequest>,
) -> Result<(StatusCode, Json<ReleaseResponse>), Failure> {
    let actor = get_current_user(&state.pool, &headers).await?;
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &name)
        .await
        .map_err(|_| json_error(ReleaseError::Unavailable))?
        .ok_or_else(|| json_error(ReleaseError::NotFound))?;
    let input = ReleaseInput {
        tag: request.tag_name.clone().unwrap_or_default(),
        target: request
            .target_commitish
            .clone()
            .filter(|target| !target.trim().is_empty())
            .unwrap_or(repository.default_branch.clone()),
        fields: ReleaseFields {
            name: request.name.clone().filter(|name| !name.is_empty()),
            body: request.body.clone().filter(|body| !body.is_empty()),
            is_draft: request.draft.unwrap_or(false),
            is_prerelease: request.prerelease.unwrap_or(false),
            make_latest: request.make_latest.unwrap_or(true),
        },
    };
    let release = releases::create(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        &state.notification_broadcaster,
        actor.id,
        &owner,
        &name,
        &input,
    )
    .await
    .map_err(json_error)?;
    let found = releases::detail_by_id(&state.pool, Some(actor.id), &owner, &name, release.id)
        .await
        .map_err(json_error)?;
    Ok((
        StatusCode::CREATED,
        Json(release_response(
            &public_origin(),
            (&owner, &name),
            &found.release,
            &found.assets,
        )),
    ))
}

/// Changes the sent fields and keeps the rest of the release as it is.
pub(super) async fn update(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id)): Path<(String, String, i64)>,
    headers: HeaderMap,
    Json(request): Json<ReleaseRequest>,
) -> Result<Json<ReleaseResponse>, Failure> {
    let actor = get_current_user(&state.pool, &headers).await?;
    let current = releases::edit_detail(&state.pool, actor.id, &owner, &name, id)
        .await
        .map_err(json_error)?;
    let kept = &current.release.release;
    let fields = ReleaseFields {
        name: request.name.clone().map_or_else(
            || kept.name.clone(),
            |name| Some(name).filter(|name| !name.is_empty()),
        ),
        body: request.body.clone().map_or_else(
            || kept.body.clone(),
            |body| Some(body).filter(|body| !body.is_empty()),
        ),
        is_draft: request.draft.unwrap_or(kept.is_draft),
        is_prerelease: request.prerelease.unwrap_or(kept.is_prerelease),
        make_latest: request.make_latest.unwrap_or(kept.is_latest),
    };
    let release = releases::update(
        &state.pool,
        &state.notification_broadcaster,
        actor.id,
        &owner,
        &name,
        id,
        &fields,
    )
    .await
    .map_err(json_error)?;
    let found = releases::detail_by_id(&state.pool, Some(actor.id), &owner, &name, release.id)
        .await
        .map_err(json_error)?;
    Ok(Json(release_response(
        &public_origin(),
        (&owner, &name),
        &found.release,
        &found.assets,
    )))
}

pub(super) async fn delete(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id)): Path<(String, String, i64)>,
    headers: HeaderMap,
) -> Result<StatusCode, Failure> {
    let actor = get_current_user(&state.pool, &headers).await?;
    releases::delete(&state.pool, &state.storage, actor.id, &owner, &name, id)
        .await
        .map_err(json_error)?;
    Ok(StatusCode::NO_CONTENT)
}

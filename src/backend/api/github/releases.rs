//! Release reads and writes retain draft visibility and tag publication in the repository domain.
use super::{auth, pagination::Pagination, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db,
    repositories::releases::{self, ReleaseError},
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

pub(super) fn release_error(error: ReleaseError) -> Error {
    use crate::backend::repositories::access::AccessError;
    match error {
        ReleaseError::Access(AccessError::Unavailable) | ReleaseError::Unavailable => {
            Error::unavailable(())
        }
        ReleaseError::Access(AccessError::NotFound) | ReleaseError::NotFound => Error::missing(),
        ReleaseError::Access(_) => Error::new(StatusCode::FORBIDDEN, "Repository write denied"),
        ReleaseError::Invalid(message) => Error::invalid(message),
        ReleaseError::TagTaken => Error::invalid("A release with this tag already exists"),
        ReleaseError::AssetNameTaken => Error::invalid("An asset with this name already exists"),
        ReleaseError::Conflict => Error::new(StatusCode::CONFLICT, "Release changed"),
        ReleaseError::TargetRequired => Error::invalid("target_commitish is required"),
        ReleaseError::Git(error) => super::contents::store_error(error),
        ReleaseError::Ref(error) => super::refs::ref_error(error),
        ReleaseError::Upload(crate::backend::files::UploadError::Invalid) => {
            Error::invalid("Invalid asset upload")
        }
        ReleaseError::Upload(crate::backend::files::UploadError::Quota) => {
            Error::new(StatusCode::FORBIDDEN, "Upload quota exhausted")
        }
        ReleaseError::Upload(_) | ReleaseError::Download(_) => Error::unavailable(()),
    }
}

pub(super) async fn asset_document(
    state: &AppState,
    owner: &str,
    name: &str,
    release: &db::models::Release,
    asset: &db::models::ReleaseAsset,
) -> Result<Value> {
    crate::backend::webhooks::documents::release_asset(&state.pool, owner, name, release, asset)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)
}

pub(super) async fn document(
    state: &AppState,
    owner: &str,
    name: &str,
    detail: &releases::ReleaseDetail,
) -> Result<Value> {
    let release = &detail.release.release;
    let author = payloads::Person {
        id: release.author_id,
        login: detail.release.author.clone(),
        site_admin: false,
    };
    let mut assets = Vec::new();
    for asset in &detail.assets {
        assets.push(asset_document(state, owner, name, release, asset).await?);
    }
    let mut value = payloads::release_json(release, &author, owner, name, Some(assets));
    if let Some(number) = db::releases::release_discussion(&state.pool, release.id)
        .await
        .map_err(Error::unavailable)?
        .and_then(|discussion| discussion.number)
    {
        value["discussion_url"] = Value::String(format!(
            "{}/{}/{}/discussions/{number}",
            crate::backend::api::http::config::link_origin(),
            codeyard_webui::encode_component(owner),
            codeyard_webui::encode_component(name)
        ));
    }
    let counts = db::reactions::reaction_counts(&state.pool, "release", &[release.id], 0)
        .await
        .map_err(Error::unavailable)?;
    value["reactions"] = payloads::reaction_rollup(
        &format!(
            "{}/releases/{}/reactions",
            super::repo_url(owner, name),
            release.id
        ),
        &counts,
    );
    Ok(value)
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let viewer = auth.as_ref().map(|auth| auth.user.id);
    let (offset, limit) = page.window()?;
    let listing = releases::list(
        &state.pool,
        viewer,
        repo.id,
        (offset / limit + 1) as i64,
        limit as i64,
    )
    .await
    .map_err(release_error)?;
    let mut values = Vec::new();
    let mut grouped = HashMap::<i64, Vec<_>>::new();
    for asset in listing.assets {
        grouped.entry(asset.release_id).or_default().push(asset);
    }
    for row in listing.releases {
        let assets = grouped.remove(&row.release.id).unwrap_or_default();
        let detail = releases::ReleaseDetail {
            repository: listing.repository.clone(),
            can_write: listing.can_write,
            release: row,
            assets,
        };
        values.push(document(&state, &owner, &name, &detail).await?);
    }
    page.response(&uri, values, (offset + limit) < listing.total as usize)
}

async fn get_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let (owner, name) = (&params["owner"], &params["repo"]);
    let (repo, auth) = repository(&state, &headers, owner, name, false).await?;
    let viewer = auth.as_ref().map(|auth| auth.user.id);
    let detail = if let Some(tag) = params.get("tag") {
        releases::detail_by_tag(&state.pool, viewer, repo.id, tag).await
    } else {
        let id = match params.get("release_id") {
            Some(id) => id.parse().map_err(|_| Error::missing())?,
            None => {
                db::releases::latest_release(&state.pool, repo.id)
                    .await
                    .map_err(Error::unavailable)?
                    .ok_or_else(Error::missing)?
                    .id
            }
        };
        releases::detail_by_id(&state.pool, viewer, repo.id, id).await
    }
    .map_err(release_error)?;
    Ok(Json(document(&state, owner, name, &detail).await?))
}

#[derive(Deserialize)]
struct Create {
    tag_name: String,
    target_commitish: Option<String>,
    name: Option<String>,
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    make_latest: Option<String>,
    #[serde(default)]
    generate_release_notes: bool,
    discussion_category_name: Option<String>,
}

#[derive(Deserialize)]
struct GenerateNotes {
    tag_name: String,
    target_commitish: Option<String>,
    previous_tag_name: Option<String>,
    configuration_file_path: Option<String>,
}
async fn generate_notes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<GenerateNotes>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let notes = releases::generate_notes(
        &state.pool,
        state.cluster.as_ref(),
        actor.id,
        repo.id,
        &releases::NotesInput {
            tag: &input.tag_name,
            target: input.target_commitish.as_deref(),
            previous_tag: input.previous_tag_name.as_deref(),
            configuration_path: input.configuration_file_path.as_deref(),
        },
    )
    .await
    .map_err(release_error)?;
    Ok(Json(payloads::release_notes(&notes)))
}

/// Whether a release may create its missing tag. A new tag queues push workflows, so a
/// credential without the workflows permission may only release existing tags. A job's
/// own events start no workflows, so a job token may create them.
async fn may_create_tag(state: &AppState, auth: &auth::Authentication) -> Result<bool> {
    Ok(auth.is_job()
        || crate::backend::identity::oauth_apps::allows_workflows(&state.pool, auth.token.id)
            .await
            .map_err(Error::unavailable)?)
}

/// The category `discussion_category_name` names. As on GitHub, an unknown category, or any
/// category while discussions are off, is refused before the release is saved.
async fn discussion_category(
    state: &AppState,
    repo_id: i64,
    name: Option<&str>,
) -> Result<Option<i64>> {
    let Some(name) = name.filter(|name| !name.is_empty()) else {
        return Ok(None);
    };
    let pool = &state.pool;
    let category = if db::repository_discussions::enabled(pool, repo_id)
        .await
        .map_err(Error::unavailable)?
    {
        db::repository_discussions::categories(pool, repo_id)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .find(|category| {
                category.name.eq_ignore_ascii_case(name) || category.slug.eq_ignore_ascii_case(name)
            })
    } else {
        None
    };
    category
        .map(|category| Some(category.id))
        .ok_or_else(|| Error::validation("Release", "discussion_category_name", "invalid"))
}

/// Records the requested category and, once the release is published, starts its discussion.
async fn start_discussion(
    state: &AppState,
    auth: &auth::Authentication,
    release: &db::models::Release,
    category: Option<i64>,
) -> Result<()> {
    let pool = &state.pool;
    if let Some(category) = category {
        db::releases::set_release_discussion_category(pool, release.id, category)
            .await
            .map_err(Error::unavailable)?;
    }
    if release.is_draft {
        return Ok(());
    }
    let Some(pending) = db::releases::release_discussion(pool, release.id)
        .await
        .map_err(Error::unavailable)?
        .filter(|pending| pending.number.is_none())
    else {
        return Ok(());
    };
    let title = release
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&release.tag_name);
    let id = db::repository_discussions::write(
        pool,
        auth.user.id,
        &auth.credential(),
        release.repo_id,
        None,
        &db::repository_discussions::Write::Create {
            category: pending.category_id,
            title,
            body: release.body.as_deref().unwrap_or_default(),
        },
    )
    .await
    .map_err(|error| {
        use db::repository_discussions::Refusal;
        match error.downcast_ref::<Refusal>() {
            Some(Refusal::Denied) => Error::new(StatusCode::FORBIDDEN, "Discussion write denied"),
            Some(Refusal::Invalid | Refusal::Missing) => {
                Error::validation("Release", "discussion_category_name", "invalid")
            }
            None => Error::unavailable(error),
        }
    })?;
    db::releases::link_release_discussion(pool, release.id, id)
        .await
        .map_err(Error::unavailable)?;
    Ok(())
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Create>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(auth::required)?;
    // The release code refuses a new tag where the tag is made.
    let new_tag_denied = !may_create_tag(&state, &auth).await?;
    let category =
        discussion_category(&state, repo.id, input.discussion_category_name.as_deref()).await?;
    let actor = &auth.user;
    let latest = match input.make_latest.as_deref() {
        Some("false") => releases::LatestSelection::Keep,
        Some("legacy") => releases::LatestSelection::Legacy,
        Some("true") | None => releases::LatestSelection::Promote,
        _ => return Err(Error::invalid("Invalid make_latest")),
    };
    let (title, body, target) = if input.generate_release_notes {
        let notes = releases::generate_notes(
            &state.pool,
            state.cluster.as_ref(),
            actor.id,
            repo.id,
            &releases::NotesInput {
                tag: &input.tag_name,
                target: input.target_commitish.as_deref(),
                previous_tag: None,
                configuration_path: None,
            },
        )
        .await
        .map_err(release_error)?;
        let body = notes
            .with_preface(input.body.as_deref())
            .map_err(release_error)?;
        (input.name.or(Some(notes.name)), Some(body), notes.target)
    } else {
        (
            input.name,
            input.body,
            input.target_commitish.unwrap_or(repo.default_branch),
        )
    };
    let release = releases::create(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        &state.notification_broadcaster,
        actor.id,
        &owner,
        &name,
        &releases::ReleaseInput {
            repository_id: repo.id,
            tag: input.tag_name,
            target,
            fields: releases::ReleaseFields {
                name: title,
                body,
                is_draft: input.draft,
                is_prerelease: input.prerelease,
                make_latest: latest,
            },
            new_tag_denied,
        },
    )
    .await
    .map_err(release_error)?;
    start_discussion(&state, &auth, &release, category).await?;
    let detail = releases::detail_by_id(&state.pool, Some(actor.id), repo.id, release.id)
        .await
        .map_err(release_error)?;
    Ok((
        StatusCode::CREATED,
        Json(document(&state, &owner, &name, &detail).await?),
    )
        .into_response())
}

#[derive(Deserialize)]
struct Update {
    name: Option<String>,
    body: Option<String>,
    draft: Option<bool>,
    prerelease: Option<bool>,
    make_latest: Option<String>,
    tag_name: Option<String>,
    target_commitish: Option<String>,
    discussion_category_name: Option<String>,
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Json(value): Json<Value>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(auth::required)?;
    let input: Update = serde_json::from_value(value.clone())
        .map_err(|_| Error::invalid("Invalid release fields"))?;
    // Publishing a draft creates its tag when the tag is missing.
    let new_tag_denied = !may_create_tag(&state, &auth).await?;
    let category =
        discussion_category(&state, repo.id, input.discussion_category_name.as_deref()).await?;
    let actor = &auth.user;
    let latest = match input.make_latest.as_deref() {
        None => None,
        Some("true") => Some(releases::LatestSelection::Promote),
        Some("legacy") => Some(releases::LatestSelection::Legacy),
        Some("false") => Some(releases::LatestSelection::Keep),
        _ => return Err(Error::invalid("Invalid make_latest")),
    };
    releases::update(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        &state.notification_broadcaster,
        actor.id,
        &owner,
        &name,
        id,
        &releases::ReleaseChanges {
            tag: input.tag_name,
            target: input.target_commitish,
            name: value.get("name").map(|_| input.name),
            body: value.get("body").map(|_| input.body),
            is_draft: input.draft,
            is_prerelease: input.prerelease,
            make_latest: latest,
            new_tag_denied,
            ..Default::default()
        },
    )
    .await
    .map_err(release_error)?;
    let detail = releases::detail_by_id(&state.pool, Some(actor.id), repo.id, id)
        .await
        .map_err(release_error)?;
    start_discussion(&state, &auth, &detail.release.release, category).await?;
    Ok(Json(document(&state, &owner, &name, &detail).await?))
}

#[derive(Deserialize)]
struct Upload {
    name: String,
    label: Option<String>,
}
async fn upload(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Query(input): Query<Upload>,
    request: Request,
) -> Result<Response> {
    let (repo, auth) = repository(&state, request.headers(), &owner, &name, true).await?;
    let auth = auth.ok_or_else(auth::required)?;
    crate::backend::repositories::access::check_write(&state.pool, repo.id, auth.user.id)
        .await
        .map_err(|error| release_error(ReleaseError::Access(error)))?;
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let _slot = state
        .upload_reads
        .clone()
        .try_acquire_owned()
        .map_err(Error::unavailable)?;
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        axum::body::to_bytes(request.into_body(), releases::MAX_ASSET_BYTES),
    )
    .await
    .map_err(Error::unavailable)?
    .map_err(|_| Error::invalid("Asset exceeds 100 MiB"))?
    .to_vec();
    auth.require_live(&state.pool).await?;
    auth.repository_scope(&state.pool, repo.id, "contents", true)
        .await
        .map_err(auth::scope_error)?;
    let actor = &auth.user;
    let asset = releases::upload_asset(
        &state.pool,
        &state.storage,
        actor.id,
        &auth.credential(),
        &owner,
        &name,
        id,
        releases::AssetUpload {
            token: Some(auth.token.id),
            name: &input.name,
            label: input.label.as_deref(),
            content_type: &content_type,
            bytes,
        },
    )
    .await
    .map_err(release_error)?;
    let detail = releases::detail_by_id(&state.pool, Some(actor.id), repo.id, id)
        .await
        .map_err(release_error)?;
    Ok((
        StatusCode::CREATED,
        Json(asset_document(&state, &owner, &name, &detail.release.release, &asset).await?),
    )
        .into_response())
}

#[derive(Deserialize)]
struct AssetUpdate {
    name: Option<String>,
    label: Option<String>,
}
async fn update_asset(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Json(value): Json<Value>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let input: AssetUpdate = serde_json::from_value(value.clone())
        .map_err(|_| Error::invalid("Invalid asset fields"))?;
    let asset = releases::update_asset(
        &state.pool,
        actor.id,
        &owner,
        &name,
        id,
        &db::releases::AssetChanges {
            name: input.name,
            label: value.get("label").map(|_| input.label),
        },
    )
    .await
    .map_err(release_error)?;
    let release = db::releases::get_release(&state.pool, repo.id, asset.release_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(Json(
        asset_document(&state, &owner, &name, &release.release, &asset).await?,
    ))
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let (_, auth) = repository(&state, &headers, &owner, &name, true).await?;
    releases::delete(
        &state.pool,
        &state.storage,
        auth.ok_or_else(auth::required)?.user.id,
        &owner,
        &name,
        id,
    )
    .await
    .map_err(release_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_assets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let detail = releases::detail_by_id(
        &state.pool,
        auth.as_ref().map(|auth| auth.user.id),
        repo.id,
        id,
    )
    .await
    .map_err(release_error)?;
    let (offset, limit) = page.window()?;
    let next = detail.assets.len() > offset + limit;
    let mut values = Vec::new();
    for asset in detail.assets.iter().skip(offset).take(limit) {
        values.push(asset_document(&state, &owner, &name, &detail.release.release, asset).await?);
    }
    page.response(&uri, values, next)
}

async fn asset(
    State(state): State<Arc<AppState>>,
    method: axum::http::Method,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let viewer = auth.as_ref().map(|auth| auth.user.id);
    let asset = db::releases::get_repository_release_asset(&state.pool, repo.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let detail = releases::detail_by_id(&state.pool, viewer, repo.id, asset.release_id)
        .await
        .map_err(release_error)?;
    if headers
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/octet-stream"))
    {
        let download = releases::download_by_id(
            &state.pool,
            &state.storage,
            viewer,
            repo.id,
            asset.id,
            // A HEAD request delivers no asset, so only GET counts as a download.
            method == axum::http::Method::GET,
        )
        .await
        .map_err(release_error)?;
        let mut response = crate::backend::api::files::download::asset_response(download, method)
            .map_err(|(status, Json(error))| Error::new(status, error.message))?;
        if response.status() == StatusCode::TEMPORARY_REDIRECT {
            *response.status_mut() = StatusCode::FOUND;
        }
        return Ok(response);
    }
    Ok(
        Json(asset_document(&state, &owner, &name, &detail.release.release, &asset).await?)
            .into_response(),
    )
}

async fn remove_asset(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth.ok_or_else(auth::required)?.user.id;
    let asset = db::releases::get_repository_release_asset(&state.pool, repo.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    releases::delete_asset(
        &state.pool,
        &state.storage,
        actor,
        &owner,
        &name,
        asset.release_id,
        asset.id,
    )
    .await
    .map_err(release_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/list-releases", get(list)),
        ("repos/get-release", get(get_one)),
        ("repos/get-release-by-tag", get(get_one)),
        ("repos/get-latest-release", get(get_one)),
        ("repos/create-release", post(create)),
        ("repos/generate-release-notes", post(generate_notes)),
        ("repos/update-release", patch(update)),
        ("repos/upload-release-asset", post(upload)),
        ("repos/update-release-asset", patch(update_asset)),
        ("repos/delete-release", delete(remove)),
        ("repos/list-release-assets", get(list_assets)),
        ("repos/get-release-asset", get(asset)),
        ("repos/delete-release-asset", delete(remove_asset)),
    ]
}

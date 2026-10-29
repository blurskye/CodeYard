//! Package metadata and recovery use the same records and authority as the registries.
use super::{auth, pagination::Pagination, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        packages::{Owner, Package, Permission, Refusal},
    },
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

fn error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Missing) => Error::missing(),
        Some(Refusal::Unauthorized) => auth::required(),
        Some(Refusal::Forbidden) => {
            Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token")
        }
        Some(Refusal::Conflict) => Error::new(StatusCode::CONFLICT, "Package cannot be restored"),
        Some(Refusal::Blob | Refusal::Offset) => Error::invalid("Invalid package data"),
        None => Error::unavailable(error),
    }
}

async fn namespace(
    state: &AppState,
    headers: &HeaderMap,
    params: &HashMap<String, String>,
    write: bool,
) -> Result<(Owner, Option<auth::Authentication>)> {
    let auth = auth::authenticate(
        state,
        headers,
        write || (!params.contains_key("org") && !params.contains_key("user_id")),
    )
    .await?;
    if write {
        auth.as_ref().ok_or_else(auth::required)?.require_write()?;
    }
    let (name, kind) = if let Some(org) = params.get("org") {
        (org.as_str(), "org")
    } else if let Some(user) = params.get("user_id") {
        (user.as_str(), "user")
    } else {
        (
            auth.as_ref()
                .ok_or_else(auth::required)?
                .user
                .username
                .as_str(),
            "user",
        )
    };
    let owner = db::packages::owner(&state.pool, name, Some(kind))
        .await
        .map_err(error)?
        .ok_or_else(Error::missing)?;
    Ok((owner, auth))
}

fn kind(kind: Option<&str>) -> Result<&str> {
    match kind {
        Some(kind) if crate::backend::packages::valid_kind(kind) => Ok(kind),
        _ => Err(Error::validation("Package", "package_type", "invalid")),
    }
}

async fn package(
    state: &AppState,
    owner: &Owner,
    params: &HashMap<String, String>,
    auth: Option<&auth::Authentication>,
    deleted: bool,
) -> Result<Package> {
    let kind = kind(params.get("package_type").map(String::as_str))?;
    let name = params.get("package_name").ok_or_else(Error::missing)?;
    db::packages::find(
        &state.pool,
        owner,
        kind,
        name,
        auth.map(|auth| auth.token.id.into()),
        Permission::Read,
        deleted,
    )
    .await
    .map_err(error)
}

async fn document(state: &AppState, owner: &Owner, package: &Package) -> Result<Value> {
    let count = db::packages::version_count(&state.pool, package.id)
        .await
        .map_err(error)?;
    let repository = if let Some(id) = package.repo_id {
        let repository = db::repositories::records::get_repository_by_id(&state.pool, id)
            .await
            .map_err(error)?
            .ok_or_else(Error::missing)?;
        Some(payloads::repository(&repository, &owner.name))
    } else {
        None
    };
    Ok(payloads::package(owner, package, count, repository))
}

#[derive(Deserialize)]
struct List {
    package_type: Option<String>,
    visibility: Option<String>,
    state: Option<String>,
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<List>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (owner, auth) = namespace(&state, &headers, &params, false).await?;
    let (offset, limit) = page.window()?;
    let token = auth.as_ref().map(|auth| auth.token.id.into());
    let mut items = Vec::new();
    if params.contains_key("package_name") {
        let package = package(&state, &owner, &params, auth.as_ref(), false).await?;
        let deleted = match query.state.as_deref() {
            None | Some("active") => false,
            Some("deleted") => true,
            _ => return Err(Error::validation("PackageVersion", "state", "invalid")),
        };
        let versions = db::packages::versions(
            &state.pool,
            package.id,
            token,
            deleted,
            offset as i64,
            limit as i64 + 1,
        )
        .await
        .map_err(error)?;
        for version in versions {
            let tags = db::packages::tags(&state.pool, version.id)
                .await
                .map_err(error)?;
            items.push(payloads::package_version(&owner, &package, &version, &tags));
        }
    } else {
        let kind = kind(query.package_type.as_deref())?;
        if query
            .visibility
            .as_deref()
            .is_some_and(|visibility| !matches!(visibility, "public" | "private"))
        {
            return Err(Error::validation("Package", "visibility", "invalid"));
        }
        let packages = db::packages::list(
            &state.pool,
            token,
            db::packages::Listing {
                owner: Some(&owner),
                repo: None,
                kind: Some(kind),
                visibility: query.visibility.as_deref(),
                offset: offset as i64,
                limit: limit as i64 + 1,
            },
        )
        .await
        .map_err(error)?;
        for package in packages {
            items.push(document(&state, &owner, &package).await?);
        }
    }
    let has_next = items.len() > limit;
    page.response(&uri, items, has_next)
}

async fn docker_conflicts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Result<Json<Vec<Value>>> {
    auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    namespace(&state, &headers, &params, false).await?;
    // Codeyard publishes directly into the container registry. It has no legacy Docker
    // registry or migration queue, so no package can have a Docker migration conflict.
    Ok(Json(Vec::new()))
}

async fn get_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let (owner, auth) = namespace(&state, &headers, &params, false).await?;
    let package = package(&state, &owner, &params, auth.as_ref(), false).await?;
    let value = if let Some(id) = params.get("package_version_id") {
        let id = id.parse().map_err(|_| Error::missing())?;
        let version = db::packages::version_by_id(
            &state.pool,
            package.id,
            auth.as_ref().map(|auth| auth.token.id.into()),
            id,
        )
        .await
        .map_err(error)?;
        let tags = db::packages::tags(&state.pool, version.id)
            .await
            .map_err(error)?;
        payloads::package_version(&owner, &package, &version, &tags)
    } else {
        document(&state, &owner, &package).await?
    };
    Ok(Json(value))
}

async fn change(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let restore = uri.path().ends_with("/restore");
    let (owner, auth) = namespace(&state, &headers, &params, true).await?;
    let auth = auth.ok_or_else(auth::required)?;
    let package = package(&state, &owner, &params, Some(&auth), restore).await?;
    let version = params
        .get("package_version_id")
        .map(|id| id.parse::<i64>().map_err(|_| Error::missing()))
        .transpose()?;
    db::packages::set_deleted(
        &state.pool,
        package.id,
        version,
        auth.token.id.into(),
        restore,
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "packages/list-docker-migration-conflicting-packages-for-organization",
            get(docker_conflicts),
        ),
        (
            "packages/list-docker-migration-conflicting-packages-for-authenticated-user",
            get(docker_conflicts),
        ),
        (
            "packages/list-docker-migration-conflicting-packages-for-user",
            get(docker_conflicts),
        ),
        ("packages/list-packages-for-organization", get(list)),
        ("packages/get-package-for-organization", get(get_one)),
        ("packages/delete-package-for-org", delete(change)),
        ("packages/restore-package-for-org", post(change)),
        (
            "packages/get-all-package-versions-for-package-owned-by-org",
            get(list),
        ),
        (
            "packages/get-package-version-for-organization",
            get(get_one),
        ),
        ("packages/delete-package-version-for-org", delete(change)),
        ("packages/restore-package-version-for-org", post(change)),
        ("packages/list-packages-for-authenticated-user", get(list)),
        ("packages/get-package-for-authenticated-user", get(get_one)),
        (
            "packages/delete-package-for-authenticated-user",
            delete(change),
        ),
        (
            "packages/restore-package-for-authenticated-user",
            post(change),
        ),
        (
            "packages/get-all-package-versions-for-package-owned-by-authenticated-user",
            get(list),
        ),
        (
            "packages/get-package-version-for-authenticated-user",
            get(get_one),
        ),
        (
            "packages/delete-package-version-for-authenticated-user",
            delete(change),
        ),
        (
            "packages/restore-package-version-for-authenticated-user",
            post(change),
        ),
        ("packages/list-packages-for-user", get(list)),
        ("packages/get-package-for-user", get(get_one)),
        ("packages/delete-package-for-user", delete(change)),
        ("packages/restore-package-for-user", post(change)),
        (
            "packages/get-all-package-versions-for-package-owned-by-user",
            get(list),
        ),
        ("packages/get-package-version-for-user", get(get_one)),
        ("packages/delete-package-version-for-user", delete(change)),
        ("packages/restore-package-version-for-user", post(change)),
    ]
}

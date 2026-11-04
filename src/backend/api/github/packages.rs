//! Package metadata and recovery use the same records and authority as the registries.

use crate::backend::api::github::pagination::Pagination;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use crate::backend::db::packages::Owner;
use crate::backend::db::packages::Package;
use crate::backend::db::packages::Permission;
use crate::backend::db::packages::Refusal;

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
        Some(Refusal::Unauthorized) => crate::backend::api::github::auth::required(),
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
) -> Result<(
    Owner,
    Option<crate::backend::api::github::auth::Authentication>,
)> {
    let auth = crate::backend::api::github::auth::authenticate(
        state,
        headers,
        write || (!params.contains_key("org") && !params.contains_key("user_id")),
    )
    .await?;
    if write {
        auth.as_ref()
            .ok_or_else(crate::backend::api::github::auth::required)?
            .require_write()?;
    }
    let (name, kind) = if let Some(org) = params.get("org") {
        (org.as_str(), "org")
    } else if let Some(user) = params.get("user_id") {
        (user.as_str(), "user")
    } else {
        (
            auth.as_ref()
                .ok_or_else(crate::backend::api::github::auth::required)?
                .user
                .username
                .as_str(),
            "user",
        )
    };
    let owner = crate::backend::db::packages::owner(&state.pool, name, Some(kind))
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
    auth: Option<&crate::backend::api::github::auth::Authentication>,
    deleted: bool,
) -> Result<Package> {
    let kind = kind(params.get("package_type").map(String::as_str))?;
    let name = params.get("package_name").ok_or_else(Error::missing)?;
    crate::backend::db::packages::find(
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

async fn documents(
    state: &AppState,
    owner: &Owner,
    packages: &[Package],
    auth: Option<&crate::backend::api::github::auth::Authentication>,
) -> Result<Vec<Value>> {
    let ids: Vec<_> = packages.iter().map(|package| package.id).collect();
    let counts: HashMap<_, _> = crate::backend::db::packages::version_counts(&state.pool, &ids)
        .await
        .map_err(error)?
        .into_iter()
        .collect();
    let ids: Vec<_> = packages
        .iter()
        .filter_map(|package| package.repo_id)
        .collect();
    let snapshots = crate::backend::db::repository_stats::readable_snapshots(
        &state.pool,
        auth.map(|auth| auth.user.id),
        &ids,
    )
    .await
    .map_err(error)?;
    let mut repositories = HashMap::new();
    for snapshot in snapshots {
        if snapshot.repository.is_private {
            if let Some(auth) = auth {
                match auth
                    .repository_scope(&state.pool, snapshot.repository.id, "metadata", false)
                    .await
                {
                    Ok(()) => {}
                    Err(
                        crate::backend::api::github::auth::ScopeError::Missing
                        | crate::backend::api::github::auth::ScopeError::Denied,
                    ) => continue,
                    Err(error) => {
                        return Err(crate::backend::api::github::auth::scope_error(error))
                    }
                }
            }
        }
        repositories.insert(
            snapshot.repository.id,
            crate::backend::webhooks::payloads::repository_snapshot(&snapshot),
        );
    }
    Ok(packages
        .iter()
        .map(|package| {
            crate::backend::webhooks::payloads::package(
                owner,
                package,
                counts.get(&package.id).copied().unwrap_or(0),
                package
                    .repo_id
                    .and_then(|id| repositories.get(&id).cloned()),
            )
        })
        .collect())
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
    let has_next;
    if params.contains_key("package_name") {
        let package = package(&state, &owner, &params, auth.as_ref(), false).await?;
        let deleted = match query.state.as_deref() {
            None | Some("active") => false,
            Some("deleted") => true,
            _ => return Err(Error::validation("PackageVersion", "state", "invalid")),
        };
        let mut versions = crate::backend::db::packages::versions(
            &state.pool,
            package.id,
            token,
            deleted,
            offset as i64,
            limit as i64 + 1,
        )
        .await
        .map_err(error)?;
        has_next = versions.len() > limit;
        versions.truncate(limit);
        let ids: Vec<_> = versions.iter().map(|version| version.id).collect();
        let mut tags: HashMap<i64, Vec<String>> = HashMap::new();
        for (id, tag) in crate::backend::db::packages::version_tags(&state.pool, &ids)
            .await
            .map_err(error)?
        {
            tags.entry(id).or_default().push(tag);
        }
        for version in versions {
            items.push(crate::backend::webhooks::payloads::package_version(
                &owner,
                &package,
                &version,
                &tags.remove(&version.id).unwrap_or_default(),
            ));
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
        let mut packages = crate::backend::db::packages::list(
            &state.pool,
            token,
            crate::backend::db::packages::Listing {
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
        has_next = packages.len() > limit;
        packages.truncate(limit);
        items = documents(&state, &owner, &packages, auth.as_ref()).await?;
    }
    page.response(&uri, items, has_next)
}

async fn docker_conflicts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Result<Json<Vec<Value>>> {
    crate::backend::api::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(crate::backend::api::github::auth::required)?;
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
    let token = auth.as_ref().map(|auth| auth.token.id.into());
    let value = if let Some(id) = params.get("package_version_id") {
        let id = id.parse().map_err(|_| Error::missing())?;
        let version =
            crate::backend::db::packages::version_by_id(&state.pool, package.id, token, id)
                .await
                .map_err(error)?;
        let tags = crate::backend::db::packages::tags(&state.pool, package.id, token, version.id)
            .await
            .map_err(error)?;
        crate::backend::webhooks::payloads::package_version(&owner, &package, &version, &tags)
    } else {
        documents(
            &state,
            &owner,
            std::slice::from_ref(&package),
            auth.as_ref(),
        )
        .await?
        .pop()
        .ok_or_else(Error::missing)?
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
    let auth = auth.ok_or_else(crate::backend::api::github::auth::required)?;
    let package = package(&state, &owner, &params, Some(&auth), restore).await?;
    let version = params
        .get("package_version_id")
        .map(|id| id.parse::<i64>().map_err(|_| Error::missing()))
        .transpose()?;
    crate::backend::db::packages::set_deleted(
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

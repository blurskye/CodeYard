use super::{authenticate, Error, Result};
use crate::backend::{
    app::AppState,
    db::{self, packages::Publish},
    packages,
    storage::MAX_OBJECT_BYTES,
};
use axum::{
    body::to_bytes,
    extract::{Path, Request, State},
    http::{header, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha512};
use std::{sync::Arc, time::Duration};

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/packages/:owner/npm/", get(handle_root))
        .route("/api/packages/:owner/npm/*path", any(handle))
}

async fn handle_root() -> Response {
    Json(json!({"registry":"Codeyard Packages"})).into_response()
}

async fn handle(
    State(state): State<Arc<AppState>>,
    Path((owner, path)): Path<(String, String)>,
    request: Request,
) -> Response {
    match tokio::time::timeout(
        Duration::from_secs(120),
        dispatch(&state, &owner, &path, request),
    )
    .await
    {
        Ok(Ok(mut response)) => {
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            response
        }
        result => {
            let error = match result {
                Ok(Err(error)) => error,
                _ => Error::unavailable(),
            };
            let mut response =
                (error.0, Json(json!({"error":error.1,"reason":error.2}))).into_response();
            if error.0 == StatusCode::UNAUTHORIZED {
                response.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    "Basic realm=\"Codeyard packages\"".parse().unwrap(),
                );
            }
            response
        }
    }
}

/// Packages are scoped to their owner, as on GitHub: `@owner/name`. An unscoped name would
/// share its stored package with the scoped one.
fn package_name<'a>(owner: &str, value: &'a str) -> Result<(&'a str, &'a str)> {
    let scoped = value.strip_prefix('@').ok_or_else(Error::missing)?;
    let (scope, rest) = scoped.split_once('/').ok_or_else(Error::invalid)?;
    if scope != owner {
        return Err(Error::missing());
    }
    let (name, rest) = rest.split_once('/').unwrap_or((rest, ""));
    if !packages::valid_name(name) || name.contains('/') {
        return Err(Error::invalid());
    }
    Ok((name, rest))
}

fn valid_version(version: &str) -> bool {
    if version.len() > 128 {
        return false;
    }
    let (version, build) = version
        .split_once('+')
        .map_or((version, None), |(a, b)| (a, Some(b)));
    let (core, pre) = version
        .split_once('-')
        .map_or((version, None), |(a, b)| (a, Some(b)));
    let parts: Vec<_> = core.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|b| b.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
    {
        return false;
    }
    for (value, prerelease) in [(pre, true), (build, false)] {
        if let Some(value) = value {
            if value.split('.').any(|part| {
                part.is_empty()
                    || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    || (prerelease
                        && part.len() > 1
                        && part.starts_with('0')
                        && part.bytes().all(|b| b.is_ascii_digit()))
            }) {
                return false;
            }
        }
    }
    true
}

fn valid_tag(tag: &str) -> bool {
    !tag.is_empty() && tag.len() <= 128
        && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && semver::VersionReq::parse(tag.trim_start_matches('v')).is_err()
}

fn version_metadata(namespace: &str, slug: &str, version: &db::packages::Version) -> Result<Value> {
    if version.manifest.len() > db::packages::MAX_NPM_METADATA_BYTES {
        return Err(Error::unavailable());
    }
    let mut metadata: Value = serde_json::from_str(&version.manifest).map_err(|_| Error::unavailable())?;
    metadata["dist"]["tarball"] = json!(format!(
        "{}/api/packages/{}/npm/@{}/{}/-/{}-{}.tgz",
        crate::backend::api::http::config::link_origin(),
        crate::webui::encode_component(namespace),
        crate::webui::encode_component(namespace),
        crate::webui::encode_component(slug),
        crate::webui::encode_component(slug),
        crate::webui::encode_component(&version.name),
    ));
    Ok(metadata)
}

async fn dispatch(
    state: &AppState,
    namespace: &str,
    path: &str,
    request: Request,
) -> Result<Response> {
    let method = request.method().clone();
    let read = method == Method::GET || method == Method::HEAD;
    let auth = authenticate(state, request.headers(), !read || path == "-/whoami").await?;
    if path == "-/ping" && read {
        return Ok(Json(json!({})).into_response());
    }
    if path == "-/whoami" && read {
        return Ok(
            Json(json!({"username":auth.ok_or_else(Error::missing)?.user.username}))
                .into_response(),
        );
    }
    let (slug, rest) = package_name(namespace, path)?;
    let name = format!("@{namespace}/{slug}");
    let owner = super::owner(state, namespace).await?;
    let package = super::package(
        state,
        &owner,
        "npm",
        slug,
        auth.as_ref(),
        method == Method::PUT && rest.is_empty(),
    )
    .await?;
    let token = auth.as_ref().map(|auth| auth.token.id.into());
    if read {
        let _slot = state.storage.try_acquire_download().ok_or_else(Error::unavailable)?;
        if rest.is_empty() {
            // Versions and tags are separate reads. A publish between them can move
            // `latest` to a version this read missed, so read again when it is absent.
            let (mut versions, mut tags) = (Map::new(), Map::new());
            for _ in 0..3 {
                (versions, tags) = (Map::new(), Map::new());
                let mut before = i64::MAX;
                let mut bytes = 0usize;
                loop {
                    let rows =
                        db::packages::version_page(&state.pool, package.id, token, before, false)
                            .await
                            .map_err(Error::db)?;
                    let count = rows.len();
                    for version in rows {
                        before = version.id;
                        bytes = bytes.saturating_add(version.manifest.len() + version.name.len() + 1024);
                        if bytes > db::packages::MAX_METADATA_RESPONSE_BYTES {
                            return Err(Error::unavailable());
                        }
                        let metadata = version_metadata(namespace, slug, &version)?;
                        if metadata["name"] != name {
                            continue;
                        }
                        for tag in db::packages::tags(&state.pool, version.id)
                            .await
                            .map_err(Error::db)?
                        {
                            bytes = bytes.saturating_add(tag.len() + version.name.len() + 16);
                            if bytes > db::packages::MAX_METADATA_RESPONSE_BYTES {
                                return Err(Error::unavailable());
                            }
                            tags.insert(tag, json!(version.name));
                        }
                        versions.insert(version.name, metadata);
                    }
                    if count < 100 {
                        break;
                    }
                }
                if versions.is_empty() || tags.contains_key("latest") {
                    break;
                }
            }
            return Ok(
                Json(json!({"_id":name,"name":name,"dist-tags":tags,"versions":versions}))
                    .into_response(),
            );
        }
        if let Some(filename) = rest.strip_prefix("-/") {
            let version = filename
                .strip_prefix(&format!("{slug}-"))
                .and_then(|value| value.strip_suffix(".tgz"))
                .filter(|value| valid_version(value))
                .ok_or_else(Error::missing)?;
            let reference = version;
            let version = db::packages::version(&state.pool, package.id, token, reference)
                .await
                .map_err(Error::db)?;
            if version.name != reference {
                return Err(Error::missing());
            }
            let blob = db::packages::blob(&state.pool, package.id, token, &version.digest)
                .await
                .map_err(Error::db)?;
            return super::oci::download(state, &blob, &method, request.headers()).await;
        }
        let version = db::packages::version(&state.pool, package.id, token, rest)
            .await
            .map_err(Error::db)?;
        if valid_version(rest) && version.name != rest {
            return Err(Error::missing());
        }
        let metadata = version_metadata(namespace, slug, &version)?;
        if metadata["name"] != name {
            return Err(Error::missing());
        }
        return Ok(Json(metadata).into_response());
    }
    if method != Method::PUT || !rest.is_empty() {
        return Err(Error(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "Unsupported npm operation",
        ));
    }
    let token = auth
        .as_ref()
        .map(|auth| auth.token.id)
        .ok_or_else(Error::missing)?;
    let _slot = super::upload_slot(state)?;
    let bytes = to_bytes(
        request.into_body(),
        (MAX_OBJECT_BYTES as usize * 4 / 3) + 1024 * 1024,
    )
    .await
    .map_err(|_| {
        Error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "SIZE_INVALID",
            "Package exceeds 100 MiB",
        )
    })?;
    let input: Value = serde_json::from_slice(&bytes).map_err(|_| Error::invalid())?;
    if input["name"] != name {
        return Err(Error::invalid());
    }
    let versions = input["versions"]
        .as_object()
        .filter(|v| v.len() == 1)
        .ok_or_else(Error::invalid)?;
    let (version, metadata) = versions.iter().next().ok_or_else(Error::invalid)?;
    if metadata.to_string().len() > db::packages::MAX_NPM_METADATA_BYTES {
        return Err(Error::invalid());
    }
    if !valid_version(version)
        || metadata["version"] != version.as_str()
        || metadata["name"] != name
    {
        return Err(Error::invalid());
    }
    let attachments = input["_attachments"]
        .as_object()
        .filter(|a| a.len() == 1)
        .ok_or_else(Error::invalid)?;
    let attachment = attachments.values().next().ok_or_else(Error::invalid)?;
    let tarball = STANDARD
        .decode(attachment["data"].as_str().ok_or_else(Error::invalid)?)
        .map_err(|_| Error::invalid())?;
    if tarball.len() > MAX_OBJECT_BYTES as usize
        || attachment["length"].as_u64() != Some(tarball.len() as u64)
        || !tarball.starts_with(b"\x1f\x8b")
    {
        return Err(Error::invalid());
    }
    let sha1 = hex::encode(sha1::Sha1::digest(&tarball));
    let integrity = format!("sha512-{}", STANDARD.encode(Sha512::digest(&tarball)));
    if metadata["dist"]["shasum"]
        .as_str()
        .is_some_and(|value| value != sha1)
        || metadata["dist"]["integrity"]
            .as_str()
            .is_some_and(|value| value != integrity)
    {
        return Err(Error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "Package digest does not match",
        ));
    }
    let tags = input["dist-tags"].as_object().ok_or_else(Error::invalid)?;
    if tags.len() > 100
        || tags.iter().any(|(tag, target)| {
            !valid_tag(tag) || target != version.as_str()
        })
    {
        return Err(Error::invalid());
    }
    let tags: Vec<_> = tags.keys().cloned().collect();
    let blob = packages::store(&state.pool, &state.storage, package.id, token, tarball)
        .await
        .map_err(Error::db)?;
    let mut metadata = metadata.clone();
    let base = format!(
        "/api/packages/{}/npm/{}",
        crate::webui::encode_component(namespace),
        if name.starts_with('@') {
            format!(
                "@{}/{}",
                crate::webui::encode_component(namespace),
                crate::webui::encode_component(slug)
            )
        } else {
            crate::webui::encode_component(slug)
        }
    );
    metadata["dist"] = json!({"tarball":format!("{base}/-/{slug}-{version}.tgz"),"shasum":sha1,"integrity":integrity});
    let manifest = metadata.to_string();
    db::packages::publish(
        &state.pool,
        token,
        Publish {
            package_id: package.id,
            name: version,
            digest: &blob.digest,
            media_type: "application/json",
            manifest: &manifest,
            tags: &tags,
            blobs: std::slice::from_ref(&blob),
        },
    )
    .await
    .map_err(Error::db)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"ok":true,"id":name,"rev":version})),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    #[test]
    fn npm_versions_are_semver() {
        for version in ["1.2.3", "0.0.0", "1.2.3-rc.1+build.02"] {
            assert!(super::valid_version(version));
        }
        for version in [
            "",
            "1.2",
            "01.2.3",
            "1.2.3-01",
            "1.2.3+",
            "../1.0.0",
            "1.2.3-rc..1",
        ] {
            assert!(!super::valid_version(version));
        }
    }
}

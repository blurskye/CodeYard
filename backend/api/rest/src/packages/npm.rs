use crate::packages::authenticate;
use crate::packages::Error;
use crate::packages::Result;
use axum::body::to_bytes;
use axum::extract::Path;
use axum::extract::Request;
use axum::extract::State;
use axum::http::header;
use axum::http::Method;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::any;
use axum::routing::get;
use axum::Json;
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_app::app::AppState;
use codeyard_db_repos::packages::Publish;
use codeyard_types::object_limits::MAX_OBJECT_BYTES;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha512;
use std::sync::Arc;
use std::time::Duration;

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route("/api/packages/:owner/npm/", get(handle_root))
        .cell_route("/api/packages/:owner/npm/*path", any(handle))
}

async fn handle_root() -> Response {
    Json(json!({"registry":"Codeyard Packages"})).into_response()
}

async fn handle(
    State(state): State<Arc<AppState>>,
    Path((owner, path)): Path<(String, String)>,
    request: Request,
) -> Response {
    let anonymous = !request.headers().contains_key(header::AUTHORIZATION);
    crate::packages::finish(
        tokio::time::timeout(
            Duration::from_secs(120),
            dispatch(&state, &owner, &path, request),
        )
        .await,
        anonymous,
        crate::packages::RegistryFormat::Npm,
    )
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
    if !codeyard_repos::npm::package_name(&format!("@{scope}/{name}")) {
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
    !tag.is_empty()
        && tag.len() <= 128
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && semver::VersionReq::parse(tag.trim_start_matches('v')).is_err()
}

fn version_metadata(
    namespace: &str,
    slug: &str,
    version: &codeyard_db_repos::packages::Version,
) -> Result<Value> {
    if version.manifest.len() > codeyard_db_repos::packages::MAX_LISTED_METADATA_BYTES {
        return Err(Error::unavailable());
    }
    let mut metadata: Value =
        serde_json::from_str(&version.manifest).map_err(|_| Error::unavailable())?;
    metadata["dist"]["tarball"] = json!(format!(
        "{}/api/packages/{}/npm/@{}/{}/-/{}-{}.tgz",
        codeyard_model::http_config::link_origin(),
        codeyard_types::paths::encode_component(namespace),
        codeyard_types::paths::encode_component(namespace),
        codeyard_types::paths::encode_component(slug),
        codeyard_types::paths::encode_component(slug),
        codeyard_types::paths::encode_component(&version.name),
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
    if let Some(path) = path.strip_prefix("-/package/") {
        return dist_tags(state, namespace, path, auth.as_ref(), request).await;
    }
    let (slug, rest) = package_name(namespace, path)?;
    let name = format!("@{namespace}/{slug}");
    let owner = crate::packages::owner(state, namespace).await?;
    let package = crate::packages::package(
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
        let _slot = state
            .storage
            .try_acquire_download()
            .ok_or_else(Error::unavailable)?;
        if rest.is_empty() {
            // Versions and tags are separate reads. A publish between them can move
            // `latest` to a version this read missed, so read again when it is absent.
            let (mut versions, mut tags) = (Map::new(), Map::new());
            for _ in 0..3 {
                (versions, tags) = (Map::new(), Map::new());
                let mut before = i64::MAX;
                let mut bytes = 0usize;
                loop {
                    let rows = codeyard_db_repos::packages::version_page(
                        &state.pool,
                        package.id,
                        token,
                        before,
                        false,
                    )
                    .await
                    .map_err(Error::db)?;
                    let count = rows.len();
                    for version in rows {
                        before = version.id;
                        bytes = bytes
                            .saturating_add(version.manifest.len() + version.name.len() + 1024);
                        if bytes > codeyard_db_repos::packages::MAX_METADATA_RESPONSE_BYTES {
                            return Err(Error::unavailable());
                        }
                        let metadata = version_metadata(namespace, slug, &version)?;
                        if metadata["name"] != name {
                            continue;
                        }
                        for tag in codeyard_db_repos::packages::tags(
                            &state.pool,
                            package.id,
                            token,
                            version.id,
                        )
                        .await
                        .map_err(Error::db)?
                        {
                            bytes = bytes.saturating_add(tag.len() + version.name.len() + 16);
                            if bytes > codeyard_db_repos::packages::MAX_METADATA_RESPONSE_BYTES {
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
            let version =
                codeyard_db_repos::packages::version(&state.pool, package.id, token, reference)
                    .await
                    .map_err(Error::db)?;
            if version.name != reference {
                return Err(Error::missing());
            }
            let blob =
                codeyard_db_repos::packages::blob(&state.pool, package.id, token, &version.digest)
                    .await
                    .map_err(Error::db)?;
            return crate::packages::oci::download(state, &blob, &method, request.headers()).await;
        }
        let version = codeyard_db_repos::packages::version(&state.pool, package.id, token, rest)
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
    let _slot = crate::packages::upload_slot(state)?;
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
    if metadata.to_string().len() > codeyard_db_repos::packages::MAX_LISTED_METADATA_BYTES {
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
        || tags
            .iter()
            .any(|(tag, target)| !valid_tag(tag) || target != version.as_str())
    {
        return Err(Error::invalid());
    }
    let tags: Vec<_> = tags.keys().cloned().collect();
    let blob = codeyard_app::packages::store(
        &state.pool,
        &state.storage,
        package.id,
        token,
        tarball,
        codeyard_db_repos::packages::Charge::Upload,
    )
    .await
    .map_err(Error::db)?;
    let mut metadata = metadata.clone();
    let base = format!(
        "/api/packages/{}/npm/{}",
        codeyard_types::paths::encode_component(namespace),
        if name.starts_with('@') {
            format!(
                "@{}/{}",
                codeyard_types::paths::encode_component(namespace),
                codeyard_types::paths::encode_component(slug)
            )
        } else {
            codeyard_types::paths::encode_component(slug)
        }
    );
    metadata["dist"] = json!({"tarball":format!("{base}/-/{slug}-{version}.tgz"),"shasum":sha1,"integrity":integrity});
    let manifest = metadata.to_string();
    codeyard_db_repos::packages::publish(
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

/// `npm dist-tag`: `GET .../dist-tags` lists the tags, and `PUT` or `DELETE .../dist-tags/<tag>`
/// points a tag at a published version or removes it.
async fn dist_tags(
    state: &AppState,
    namespace: &str,
    path: &str,
    auth: Option<&codeyard_identity::github_auth::Authentication>,
    request: Request,
) -> Result<Response> {
    let (slug, rest) = package_name(namespace, path)?;
    let tag = match rest.strip_prefix("dist-tags") {
        Some("") => None,
        Some(tag) => Some(
            tag.strip_prefix('/')
                .filter(|tag| valid_tag(tag))
                .ok_or_else(Error::invalid)?,
        ),
        None => return Err(Error::missing()),
    };
    let owner = crate::packages::owner(state, namespace).await?;
    let package = crate::packages::package(state, &owner, "npm", slug, auth, false).await?;
    let method = request.method().clone();
    match (method, tag) {
        (Method::GET | Method::HEAD, None) => {
            let token = auth.map(|auth| auth.token.id.into());
            let (mut tags, mut after, mut bytes) = (Map::new(), String::new(), 0usize);
            loop {
                let page = codeyard_db_repos::packages::tag_page(
                    &state.pool,
                    package.id,
                    token,
                    &after,
                    100,
                )
                .await
                .map_err(Error::db)?;
                let done = page.len() < 100;
                for (tag, version) in page {
                    bytes = bytes.saturating_add(tag.len() + version.len() + 16);
                    if bytes > codeyard_db_repos::packages::MAX_METADATA_RESPONSE_BYTES {
                        return Err(Error::unavailable());
                    }
                    after.clone_from(&tag);
                    tags.insert(tag, json!(version));
                }
                if done {
                    return Ok(Json(Value::Object(tags)).into_response());
                }
            }
        }
        (Method::PUT, Some(tag)) => {
            let token = crate::packages::writer(auth)?.token.id;
            let bytes = to_bytes(request.into_body(), 1024)
                .await
                .map_err(|_| Error::invalid())?;
            let version: String = serde_json::from_slice(&bytes).map_err(|_| Error::invalid())?;
            if !valid_version(&version) {
                return Err(Error::invalid());
            }
            codeyard_db_repos::packages::set_tag(&state.pool, package.id, token, tag, &version)
                .await
                .map_err(Error::db)?;
            Ok(Json(json!({"ok":true})).into_response())
        }
        (Method::DELETE, Some(tag)) => {
            let token = crate::packages::writer(auth)?.token.id;
            codeyard_db_repos::packages::delete_tag(&state.pool, package.id, token, tag)
                .await
                .map_err(Error::db)?;
            Ok(Json(json!({"ok":true})).into_response())
        }
        _ => Err(Error(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "Unsupported npm operation",
        )),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn npm_versions_are_semver() {
        for version in ["1.2.3", "0.0.0", "1.2.3-rc.1+build.02"] {
            assert!(crate::packages::npm::valid_version(version));
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
            assert!(!crate::packages::npm::valid_version(version));
        }
    }
}

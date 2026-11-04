use crate::backend::api::packages::container_owner as owner;
use crate::backend::api::packages::package;

use crate::backend::api::packages::Error;
use crate::backend::api::packages::Result;
use crate::backend::app::AppState;

use codeyard_db_repos::packages::Blob;
use codeyard_db_repos::packages::Publish;
use codeyard_db_repos::packages::Upload;

use crate::backend::storage::Download;
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use codeyard_support::object_limits::MAX_OBJECT_BYTES;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v2/", any(handle))
        .route("/v2/*path", any(handle))
}

async fn handle(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let result = tokio::time::timeout(Duration::from_secs(120), dispatch(&state, request)).await;
    let mut response = match result {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => error.into_response(),
        Err(_) => Error::unavailable().into_response(),
    };
    response.headers_mut().insert(
        "docker-distribution-api-version",
        "registry/2.0".parse().unwrap(),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

fn response(status: StatusCode, location: Option<&str>, digest: Option<&str>) -> Result<Response> {
    let mut response = status.into_response();
    for (name, value) in [
        (header::LOCATION, location),
        ("docker-content-digest".parse().unwrap(), digest),
    ] {
        if let Some(value) = value {
            response
                .headers_mut()
                .insert(name, value.parse().map_err(|_| Error::invalid())?);
        }
    }
    Ok(response)
}

/// The smallest chunk an upload accepts before its last one.
const MIN_CHUNK_BYTES: i64 = 1024 * 1024;

fn upload_response(status: StatusCode, name: &str, upload: &Upload) -> Result<Response> {
    let location = format!("/v2/{name}/blobs/uploads/{}", upload.id);
    let mut response = response(status, Some(&location), None)?;
    response.headers_mut().insert(
        "docker-upload-uuid",
        upload.id.parse().map_err(|_| Error::invalid())?,
    );
    response.headers_mut().insert(
        header::RANGE,
        format!("0-{}", (upload.offset - 1).max(0)).parse().unwrap(),
    );
    response
        .headers_mut()
        .insert("oci-chunk-min-length", MIN_CHUNK_BYTES.into());
    Ok(response)
}

fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 128
        && tag
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

async fn dispatch(state: &AppState, request: Request) -> Result<Response> {
    let method = request.method().clone();
    let read = method == Method::GET || method == Method::HEAD;
    let headers = request.headers().clone();
    let path = request
        .uri()
        .path()
        .strip_prefix("/v2/")
        .ok_or_else(Error::invalid)?
        .to_owned();
    if path.is_empty() && read {
        crate::backend::api::packages::tokens::authenticate(state, &headers, "", false).await?;
        if !headers.contains_key(header::AUTHORIZATION) {
            return Err(Error(
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "Registry authentication required",
            ));
        }
        return Ok(Json(json!({})).into_response());
    }
    let query: HashMap<String, String> =
        url::form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    if path == "token" && method == Method::GET {
        return crate::backend::api::packages::tokens::exchange(
            state,
            &headers,
            request.uri().query().unwrap_or(""),
        )
        .await;
    }
    let (name, resource, reference) = if let Some((name, id)) = path.rsplit_once("/blobs/uploads/")
    {
        (name, "uploads", id)
    } else if let Some((name, id)) = path.rsplit_once("/manifests/") {
        (name, "manifests", id)
    } else if let Some((name, id)) = path.rsplit_once("/blobs/") {
        (name, "blobs", id)
    } else if let Some(name) = path.strip_suffix("/tags/list") {
        (name, "tags", "")
    } else {
        return Err(Error::missing());
    };
    let auth = crate::backend::api::packages::tokens::authenticate(
        state,
        &headers,
        name,
        !read || resource == "uploads",
    )
    .await?;
    let (namespace, package_name) = name.split_once('/').ok_or_else(Error::invalid)?;
    let owner = owner(state, namespace).await?;
    let create = (method == Method::POST && resource == "uploads" && reference.is_empty())
        || (method == Method::PUT && resource == "manifests");
    let package = package(
        state,
        &owner,
        "container",
        package_name,
        auth.as_ref(),
        create,
    )
    .await?;
    let token = auth.as_ref().map(|auth| auth.token.id.into());
    let write_token = || {
        auth.as_ref().map(|auth| auth.token.id).ok_or(Error(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Authentication required",
        ))
    };
    match (resource, method.as_str()) {
        ("blobs", "GET" | "HEAD") => {
            if !crate::backend::packages::valid_digest(reference) {
                return Err(Error::invalid());
            }
            let blob = codeyard_db_repos::packages::blob(&state.pool, package.id, token, reference)
                .await
                .map_err(Error::db)?;
            download(state, &blob, &method, &headers).await
        }
        ("uploads", "POST") if reference.is_empty() => {
            let token = write_token()?;
            if let (Some(digest), Some(from)) = (query.get("mount"), query.get("from")) {
                if crate::backend::packages::valid_digest(digest) {
                    if let Some((namespace, source)) = from.split_once('/') {
                        let mount = async {
                            let source_auth = crate::backend::api::packages::tokens::authenticate(
                                state, &headers, from, false,
                            )
                            .await?;
                            let owner =
                                crate::backend::api::packages::owner(state, namespace).await?;
                            let source = crate::backend::api::packages::package(
                                state,
                                &owner,
                                "container",
                                source,
                                source_auth.as_ref(),
                                false,
                            )
                            .await?;
                            let blob = codeyard_db_repos::packages::blob(
                                &state.pool,
                                source.id,
                                Some(token.into()),
                                digest,
                            )
                            .await
                            .map_err(Error::db)?;
                            codeyard_db_repos::packages::reserve_blob(
                                &state.pool,
                                package.id,
                                token,
                                &blob,
                                codeyard_db_repos::packages::Charge::Upload,
                            )
                            .await
                            .map_err(Error::db)?;
                            Ok::<_, Error>(blob)
                        }
                        .await;
                        match mount {
                            Ok(blob) => {
                                return response(
                                    StatusCode::CREATED,
                                    Some(&format!("/v2/{name}/blobs/{}", blob.digest)),
                                    Some(&blob.digest),
                                )
                            }
                            Err(error)
                                if matches!(
                                    error.0,
                                    StatusCode::NOT_FOUND | StatusCode::FORBIDDEN
                                ) => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
            }
            let id = uuid::Uuid::new_v4().to_string();
            let upload =
                codeyard_db_repos::packages::start_upload(&state.pool, package.id, token, &id)
                    .await
                    .map_err(Error::db)?;
            if let Some(digest) = query.get("digest") {
                finish(
                    state,
                    &package,
                    token,
                    &upload,
                    digest,
                    name,
                    &headers,
                    request.into_body(),
                )
                .await
            } else {
                // The resumable protocol starts with an empty POST; monolithic uploads carry a digest.
                let bytes = to_bytes(request.into_body(), 1)
                    .await
                    .map_err(|_| Error::invalid())?;
                if !bytes.is_empty() {
                    return Err(Error::invalid());
                }
                upload_response(StatusCode::ACCEPTED, name, &upload)
            }
        }
        ("uploads", "GET" | "HEAD" | "PATCH" | "PUT" | "DELETE") => {
            let token = write_token()?;
            let upload =
                codeyard_db_repos::packages::upload(&state.pool, package.id, token, reference)
                    .await
                    .map_err(Error::db)?;
            match method.as_str() {
                "GET" | "HEAD" => upload_response(StatusCode::NO_CONTENT, name, &upload),
                "DELETE" => {
                    codeyard_db_repos::packages::cancel_upload(
                        &state.pool,
                        package.id,
                        token,
                        reference,
                    )
                    .await
                    .map_err(Error::db)?;
                    response(StatusCode::NO_CONTENT, None, None)
                }
                "PATCH" => {
                    let _slot = crate::backend::api::packages::upload_slot(state)?;
                    let upload = append(
                        state,
                        package.id,
                        token,
                        upload,
                        &headers,
                        request.into_body(),
                        false,
                    )
                    .await?;
                    upload_response(StatusCode::ACCEPTED, name, &upload)
                }
                _ => {
                    finish(
                        state,
                        &package,
                        token,
                        &upload,
                        query.get("digest").ok_or_else(Error::invalid)?,
                        name,
                        &headers,
                        request.into_body(),
                    )
                    .await
                }
            }
        }
        ("manifests", "GET" | "HEAD") => {
            let version =
                codeyard_db_repos::packages::version(&state.pool, package.id, token, reference)
                    .await
                    .map_err(Error::db)?;
            let mut response = response(StatusCode::OK, None, Some(&version.digest))?;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                version
                    .media_type
                    .parse()
                    .map_err(|_| Error::unavailable())?,
            );
            response.headers_mut().insert(
                header::CONTENT_LENGTH,
                version.manifest.len().to_string().parse().unwrap(),
            );
            if method != Method::HEAD {
                *response.body_mut() = Body::from(version.manifest);
            }
            Ok(response)
        }
        ("manifests", "PUT") => {
            let _slot = crate::backend::api::packages::upload_slot(state)?;
            let token = write_token()?;
            if !valid_tag(reference) && !crate::backend::packages::valid_digest(reference) {
                return Err(Error::invalid());
            }
            let media_type = headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .to_owned();
            let bytes = to_bytes(request.into_body(), 4 * 1024 * 1024)
                .await
                .map_err(|_| Error::invalid())?;
            let manifest: Value = serde_json::from_slice(&bytes).map_err(|_| {
                Error(
                    StatusCode::BAD_REQUEST,
                    "MANIFEST_INVALID",
                    "Invalid manifest",
                )
            })?;
            let digest = crate::backend::packages::digest(&bytes);
            if reference.starts_with("sha256:") && reference != digest {
                return Err(Error(
                    StatusCode::BAD_REQUEST,
                    "DIGEST_INVALID",
                    "Manifest digest does not match",
                ));
            }
            let blobs = manifest_blobs(state, package.id, token, &media_type, &manifest).await?;
            let manifest_blob = crate::backend::packages::store(
                &state.pool,
                &state.storage,
                package.id,
                token,
                bytes.to_vec(),
                codeyard_db_repos::packages::Charge::Upload,
            )
            .await
            .map_err(Error::db)?;
            let mut blobs = blobs;
            blobs.push(manifest_blob);
            let tags = if reference == digest {
                vec![]
            } else {
                vec![reference.into()]
            };
            let text = std::str::from_utf8(&bytes).map_err(|_| Error::invalid())?;
            codeyard_db_repos::packages::publish(
                &state.pool,
                token,
                Publish {
                    package_id: package.id,
                    name: &digest,
                    digest: &digest,
                    media_type: &media_type,
                    manifest: text,
                    tags: &tags,
                    blobs: &blobs,
                },
            )
            .await
            .map_err(Error::db)?;
            response(
                StatusCode::CREATED,
                Some(&format!("/v2/{name}/manifests/{digest}")),
                Some(&digest),
            )
        }
        ("manifests", "DELETE") => {
            if !crate::backend::packages::valid_digest(reference) {
                return Err(Error::invalid());
            }
            let version =
                codeyard_db_repos::packages::version(&state.pool, package.id, token, reference)
                    .await
                    .map_err(Error::db)?;
            codeyard_db_repos::packages::set_deleted(
                &state.pool,
                package.id,
                Some(version.id),
                write_token()?.into(),
                false,
            )
            .await
            .map_err(Error::db)?;
            response(StatusCode::ACCEPTED, None, None)
        }
        ("tags", "GET" | "HEAD") => {
            let n: usize = query
                .get("n")
                .map(|n| n.parse())
                .transpose()
                .map_err(|_| Error::invalid())?
                .unwrap_or(100)
                .clamp(1, 1000);
            let mut tags: Vec<String> = codeyard_db_repos::packages::tag_page(
                &state.pool,
                package.id,
                token,
                query.get("last").map(String::as_str).unwrap_or(""),
                n as i64 + 1,
            )
            .await
            .map_err(Error::db)?
            .into_iter()
            .map(|(tag, _)| tag)
            .collect();
            let more = tags.len() > n;
            tags.truncate(n);
            let last = tags.last().cloned();
            let mut response = Json(json!({"name":name,"tags":tags})).into_response();
            if more {
                if let Some(last) = last {
                    response.headers_mut().insert(
                        header::LINK,
                        format!("</v2/{name}/tags/list?n={n}&last={last}>; rel=\"next\"")
                            .parse()
                            .map_err(|_| Error::invalid())?,
                    );
                }
            }
            Ok(response)
        }
        _ => Err(Error(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "Unsupported registry operation",
        )),
    }
}

async fn append(
    state: &AppState,
    package: i64,
    token: i64,
    upload: Upload,
    headers: &HeaderMap,
    body: Body,
    final_chunk: bool,
) -> Result<Upload> {
    let bytes = to_bytes(body, MAX_OBJECT_BYTES as usize)
        .await
        .map_err(|_| {
            Error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "SIZE_INVALID",
                "Layer exceeds 100 MiB",
            )
        })?;
    if let Some(range) = headers.get(header::CONTENT_RANGE) {
        let range = range
            .to_str()
            .map_err(|_| Error::invalid())?
            .strip_prefix("bytes ")
            .unwrap_or(range.to_str().map_err(|_| Error::invalid())?);
        let (start, end) = range.split_once('-').ok_or_else(Error::invalid)?;
        let start: i64 = start.parse().map_err(|_| Error::invalid())?;
        let end: i64 = end.parse().map_err(|_| Error::invalid())?;
        if start != upload.offset
            || end.checked_sub(start).and_then(|n| n.checked_add(1)) != Some(bytes.len() as i64)
        {
            return Err(Error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "RANGE_INVALID",
                "Upload offset does not match",
            ));
        }
    }
    if upload.offset + bytes.len() as i64 > MAX_OBJECT_BYTES {
        return Err(Error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "SIZE_INVALID",
            "Layer exceeds 100 MiB",
        ));
    }
    // Only the last chunk may be shorter than the advertised minimum, which bounds the chunks one
    // upload stores. Docker sends a small blob as one PATCH and an empty PUT, so the short chunk
    // is refused only once another chunk follows it.
    if !final_chunk && !bytes.is_empty() {
        let chunks: Vec<Blob> =
            serde_json::from_str(&upload.chunks).map_err(|_| Error::unavailable())?;
        if chunks
            .last()
            .is_some_and(|chunk| chunk.size < MIN_CHUNK_BYTES)
        {
            return Err(Error(
                StatusCode::BAD_REQUEST,
                "SIZE_INVALID",
                "Upload chunks must be at least 1 MiB",
            ));
        }
    }
    if bytes.is_empty() {
        return Ok(upload);
    }
    let chunk = crate::backend::packages::store(
        &state.pool,
        &state.storage,
        package,
        token,
        bytes.to_vec(),
        codeyard_db_repos::packages::Charge::Chunk(&upload.id),
    )
    .await
    .map_err(Error::db)?;
    codeyard_db_repos::packages::append_upload(
        &state.pool,
        package,
        token,
        &upload.id,
        upload.offset,
        &chunk,
    )
    .await
    .map_err(Error::db)
}

#[expect(
    clippy::too_many_arguments,
    reason = "Finishing an upload needs the package, the upload session and the request it arrived on."
)]
async fn finish(
    state: &AppState,
    package: &codeyard_db_repos::packages::Package,
    token: i64,
    upload: &Upload,
    digest: &str,
    name: &str,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response> {
    if !crate::backend::packages::valid_digest(digest) {
        return Err(Error::invalid());
    }
    let _slot = crate::backend::api::packages::upload_slot(state)?;
    let current = codeyard_db_repos::packages::upload(&state.pool, package.id, token, &upload.id)
        .await
        .map_err(Error::db)?;
    let upload = append(state, package.id, token, current, headers, body, true).await?;
    let chunks: Vec<Blob> =
        serde_json::from_str(&upload.chunks).map_err(|_| Error::unavailable())?;
    let blob = if chunks.len() == 1 && chunks[0].digest == digest {
        codeyard_db_repos::packages::blob(&state.pool, package.id, Some(token.into()), digest)
            .await
            .map_err(Error::db)?
    } else {
        let mut bytes = Vec::with_capacity(upload.offset as usize);
        for chunk in chunks {
            bytes.extend(
                crate::backend::packages::read(&state.storage, &chunk)
                    .await
                    .map_err(Error::db)?,
            );
        }
        if crate::backend::packages::digest(&bytes) != digest {
            return Err(Error(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "Blob digest does not match",
            ));
        }
        // The chunks were charged as they arrived.
        crate::backend::packages::store(
            &state.pool,
            &state.storage,
            package.id,
            token,
            bytes,
            codeyard_db_repos::packages::Charge::Assembled,
        )
        .await
        .map_err(Error::db)?
    };
    codeyard_db_repos::packages::finish_upload(
        &state.pool,
        package.id,
        token,
        &upload.id,
        upload.offset,
    )
    .await
    .map_err(Error::db)?;
    response(
        StatusCode::CREATED,
        Some(&format!("/v2/{name}/blobs/{}", blob.digest)),
        Some(&blob.digest),
    )
}

async fn manifest_blobs(
    state: &AppState,
    package: i64,
    token: i64,
    media_type: &str,
    manifest: &Value,
) -> Result<Vec<Blob>> {
    if manifest["schemaVersion"] != 2 || manifest.get("mediaType").is_some_and(|v| v != media_type)
    {
        return Err(Error::invalid());
    }
    let descriptors = match media_type {
        "application/vnd.oci.image.manifest.v1+json"
        | "application/vnd.docker.distribution.manifest.v2+json" => {
            let mut descriptors = manifest["layers"]
                .as_array()
                .ok_or_else(Error::invalid)?
                .clone();
            descriptors.push(manifest["config"].clone());
            descriptors
        }
        "application/vnd.oci.image.index.v1+json"
        | "application/vnd.docker.distribution.manifest.list.v2+json" => manifest["manifests"]
            .as_array()
            .ok_or_else(Error::invalid)?
            .clone(),
        _ => {
            return Err(Error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "MANIFEST_INVALID",
                "Unsupported manifest media type",
            ))
        }
    };
    if descriptors.len() > 1000 {
        return Err(Error::invalid());
    }
    let index = matches!(
        media_type,
        "application/vnd.oci.image.index.v1+json"
            | "application/vnd.docker.distribution.manifest.list.v2+json"
    );
    let mut blobs = Vec::new();
    for descriptor in descriptors {
        let digest = descriptor["digest"]
            .as_str()
            .filter(|digest| crate::backend::packages::valid_digest(digest))
            .ok_or_else(Error::invalid)?;
        if index {
            let child = codeyard_db_repos::packages::version(
                &state.pool,
                package,
                Some(token.into()),
                digest,
            )
            .await
            .map_err(Error::db)?;
            if descriptor["mediaType"] != child.media_type {
                return Err(Error::invalid());
            }
        }
        let size = descriptor["size"]
            .as_i64()
            .filter(|size| *size >= 0)
            .ok_or_else(Error::invalid)?;
        let foreign = matches!(
            descriptor["mediaType"].as_str(),
            Some(
                "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip"
                    | "application/vnd.oci.image.layer.nondistributable.v1.tar"
                    | "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip"
                    | "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd"
            )
        );
        if !index && foreign {
            let urls = descriptor["urls"]
                .as_array()
                .filter(|urls| !urls.is_empty())
                .ok_or_else(Error::invalid)?;
            for url in urls {
                let url = url
                    .as_str()
                    .and_then(|url| url::Url::parse(url).ok())
                    .filter(|url| {
                        matches!(url.scheme(), "http" | "https")
                            && url.host_str().is_some()
                            && url.username().is_empty()
                            && url.password().is_none()
                    })
                    .ok_or_else(Error::invalid)?;
                let _ = url;
            }
            continue;
        }
        let blob =
            codeyard_db_repos::packages::blob(&state.pool, package, Some(token.into()), digest)
                .await
                .map_err(Error::db)?;
        if blob.size != size {
            return Err(Error(
                StatusCode::BAD_REQUEST,
                "MANIFEST_BLOB_UNKNOWN",
                "Manifest blob size does not match",
            ));
        }
        blobs.push(blob);
    }
    Ok(blobs)
}

/// `validator` is the response's strong ETag, if it has one, for `If-Range`.
pub(super) fn byte_range(
    headers: &HeaderMap,
    size: u64,
    validator: Option<&str>,
) -> Result<Option<(u64, u64)>> {
    let invalid = || {
        Error(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "RANGE_INVALID",
            "Invalid blob range",
        )
    };
    let (range, partial) =
        crate::backend::api::http::range::requested(headers, validator).map_err(|_| invalid())?;
    if !partial {
        return Ok(None);
    }
    let (start, length) = range.normalize(size).map_err(|_| invalid())?;
    Ok(Some((start, start + length - 1)))
}

pub(super) async fn download(
    state: &AppState,
    blob: &Blob,
    method: &Method,
    headers: &HeaderMap,
) -> Result<Response> {
    let mut response = response(StatusCode::OK, None, Some(&blob.digest))?;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        blob.size.to_string().parse().unwrap(),
    );
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    if *method == Method::HEAD {
        return Ok(response);
    }
    let range = byte_range(headers, blob.size as u64, None)?;
    let (start, length) = range.map_or((0, blob.size as u64), |(start, end)| {
        (start, end - start + 1)
    });
    let permit = state
        .storage
        .try_acquire_download()
        .ok_or_else(Error::unavailable)?;
    match state
        .storage
        .download(
            &codeyard_db_repos::packages::object_key(&blob.digest),
            &blob.digest,
            None,
        )
        .await
        .map_err(|_| Error::unavailable())?
    {
        Download::Redirect(url) => {
            response.headers_mut().remove(header::CONTENT_LENGTH);
            *response.status_mut() = StatusCode::TEMPORARY_REDIRECT;
            response.headers_mut().insert(
                header::LOCATION,
                url.parse().map_err(|_| Error::unavailable())?,
            );
        }
        Download::Stream { mut file, bytes } => {
            if bytes != blob.size as u64 {
                return Err(Error::unavailable());
            }
            if let Some((start, end)) = range {
                *response.status_mut() = StatusCode::PARTIAL_CONTENT;
                response.headers_mut().insert(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", blob.size)
                        .parse()
                        .unwrap(),
                );
                response
                    .headers_mut()
                    .insert(header::CONTENT_LENGTH, length.to_string().parse().unwrap());
            }
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|_| Error::unavailable())?;
            let file = file.take(length);
            let stream =
                futures_util::stream::try_unfold((file, permit), |(mut file, permit)| async move {
                    let mut buffer = vec![0u8; 64 * 1024];
                    let read = file.read(&mut buffer).await?;
                    if read == 0 {
                        return Ok::<_, std::io::Error>(None);
                    }
                    buffer.truncate(read);
                    Ok(Some((bytes::Bytes::from(buffer), (file, permit))))
                });
            // The send deadline frees the download slot from a client that stops reading.
            *response.body_mut() = crate::backend::api::http::middleware::download_body(stream);
        }
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blob_ranges_are_bounded() {
        for (value, expected) in [
            ("bytes=0-4", (0, 4)),
            ("bytes=5-", (5, 9)),
            ("bytes=-3", (7, 9)),
            ("bytes=2-100", (2, 9)),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::RANGE, value.parse().unwrap());
            assert_eq!(byte_range(&headers, 10, None).unwrap(), Some(expected));
        }
        for value in [
            "bytes=10-",
            "bytes=5-2",
            "bytes=-0",
            "bytes=0-1,3-5",
            "bytes=x-y",
            "bytes=18446744073709551616-",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::RANGE, value.parse().unwrap());
            assert!(byte_range(&headers, 10, None).is_err());
        }
    }
}

//! RubyGems push and Bundler's compact index; no Ruby deserialization or extraction.
use crate::packages::authenticate;
use crate::packages::Error;
use crate::packages::Result;
use axum::body::to_bytes;
use axum::body::Body;
use axum::extract::Path;
use axum::extract::Request;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::Method;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_app::app::AppState;
use codeyard_db_repos::packages::Publish;
use codeyard_types::object_limits::MAX_OBJECT_BYTES;
use serde_json::json;
use serde_json::Value;
use serde_yaml::Value as Yaml;
use sha2::Digest;
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new().cell_route("/api/packages/:owner/rubygems/*path", any(handle))
}
async fn handle(
    State(state): State<Arc<AppState>>,
    Path((owner, path)): Path<(String, String)>,
    request: Request,
) -> Response {
    run(&state, &owner, &path, request).await
}
async fn run(state: &AppState, owner: &str, path: &str, request: Request) -> Response {
    let anonymous = !request.headers().contains_key(header::AUTHORIZATION);
    crate::packages::finish(
        tokio::time::timeout(
            Duration::from_secs(120),
            dispatch(state, owner, path, request),
        )
        .await,
        anonymous,
        crate::packages::RegistryFormat::Standard,
    )
}

fn untag(mut value: &Yaml) -> &Yaml {
    while let Yaml::Tagged(tag) = value {
        value = &tag.value;
    }
    value
}
fn field<'a>(value: &'a Yaml, key: &str) -> Option<&'a Yaml> {
    untag(value).get(key).map(untag)
}
fn string(value: &Yaml) -> Option<&str> {
    untag(value).as_str()
}
fn gem_version(value: &Yaml) -> Option<&str> {
    string(value).or_else(|| field(value, "version").and_then(string))
}
fn requirements(value: Option<&Yaml>) -> Result<String> {
    let Some(value) = value else {
        return Ok(">= 0".into());
    };
    let rows = field(value, "requirements")
        .and_then(Yaml::as_sequence)
        .ok_or_else(Error::invalid)?;
    let mut requirements = Vec::new();
    for row in rows {
        let row = untag(row)
            .as_sequence()
            .filter(|r| r.len() == 2)
            .ok_or_else(Error::invalid)?;
        let op = string(&row[0])
            .filter(|s| matches!(*s, "=" | "!=" | ">" | "<" | ">=" | "<=" | "~>"))
            .ok_or_else(Error::invalid)?;
        let version = gem_version(&row[1])
            .filter(|v| valid_version(v))
            .ok_or_else(Error::invalid)?;
        requirements.push(format!("{op} {version}"));
    }
    if requirements.is_empty() {
        requirements.push(">= 0".into());
    }
    Ok(requirements.join("&"))
}
fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 128
        && version.as_bytes()[0].is_ascii_digit()
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric()))
}
fn parse(bytes: &[u8]) -> Result<(String, String, Value)> {
    let spec = crate::packages::archives::gem_metadata(bytes)?;
    let name = field(&spec, "name")
        .and_then(string)
        .filter(|s| codeyard_app::packages::valid_package_name("rubygems", s))
        .ok_or_else(Error::invalid)?
        .to_owned();
    let version = field(&spec, "version")
        .and_then(gem_version)
        .filter(|s| valid_version(s))
        .ok_or_else(Error::invalid)?;
    let platform = if let Some(platform) = field(&spec, "platform") {
        if let Some(platform) = string(platform) {
            platform.to_owned()
        } else {
            ["cpu", "os", "version"]
                .into_iter()
                .filter_map(|key| field(platform, key).and_then(string))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("-")
        }
    } else {
        "ruby".into()
    };
    if platform.len() > 80
        || platform.is_empty()
        || !platform
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(Error::invalid());
    }
    let key = if platform == "ruby" {
        version.to_owned()
    } else {
        format!("{version}-{platform}")
    };
    let mut dependencies = Vec::new();
    if let Some(rows) = field(&spec, "dependencies").and_then(Yaml::as_sequence) {
        for dep in rows {
            let development = field(dep, "type")
                .and_then(string)
                .is_some_and(|s| matches!(s, "development" | ":development"));
            if development {
                continue;
            }
            let name = field(dep, "name")
                .and_then(string)
                .filter(|s| codeyard_app::packages::valid_package_name("rubygems", s))
                .ok_or_else(Error::invalid)?;
            dependencies.push(format!(
                "{name}:{}",
                requirements(field(dep, "requirement"))?
            ));
        }
    }
    Ok((
        name,
        key,
        json!({"version":version,"platform":platform,"dependencies":dependencies,"ruby":requirements(field(&spec,"required_ruby_version"))?,"rubygems":requirements(field(&spec,"required_rubygems_version"))?}),
    ))
}
fn metadata(version: &codeyard_db_repos::packages::Version) -> Result<Value> {
    serde_json::from_str(&version.manifest).map_err(|_| Error::unavailable())
}
fn info_body(versions: &[codeyard_db_repos::packages::Version]) -> Result<String> {
    let mut body = String::from("---\n");
    for version in versions {
        let meta = metadata(version)?;
        let dependencies = meta["dependencies"]
            .as_array()
            .ok_or_else(Error::unavailable)?
            .iter()
            .map(|s| s.as_str().ok_or_else(Error::unavailable))
            .collect::<Result<Vec<_>>>()?
            .join(",");
        let ruby = meta["ruby"].as_str().ok_or_else(Error::unavailable)?;
        let rubygems = meta["rubygems"].as_str().ok_or_else(Error::unavailable)?;
        body.push_str(&format!(
            "{} {dependencies}|checksum:{},ruby:{ruby},rubygems:{rubygems}\n",
            version.name,
            version.digest.trim_start_matches("sha256:")
        ));
    }
    Ok(body)
}
fn md5(bytes: &[u8]) -> String {
    hex::encode(md5::Md5::digest(bytes))
}
fn compact(body: String, method: &Method, headers: &HeaderMap) -> Result<Response> {
    let bytes = body.into_bytes();
    let etag = format!("\"{}\"", md5(&bytes));
    if codeyard_api_http::static_files::not_modified(headers, &etag) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }
    let mut start = 0;
    let mut end = bytes.len();
    let mut status = StatusCode::OK;
    // Indexes are rebuilt, not append-only: a changed ETag must replace Bundler's cache,
    // including after deletion or insertion in the middle of a version list.
    if !headers.contains_key(header::IF_NONE_MATCH) {
        // Ignoring an unsatisfiable range sends a complete new index instead of stranding
        // a client whose old cached index was longer than the current one.
        if let Ok(Some((first, last))) =
            crate::packages::oci::byte_range(headers, bytes.len() as u64, Some(&etag))
        {
            start = first as usize;
            end = last as usize + 1;
            status = StatusCode::PARTIAL_CONTENT;
        }
    }
    let mut response = Response::new(if *method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(bytes[start..end].to_vec())
    });
    *response.status_mut() = status;
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        "text/plain; charset=utf-8".parse().unwrap(),
    );
    h.insert(header::ETAG, etag.parse().unwrap());
    h.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    h.insert(
        header::CONTENT_LENGTH,
        (end - start).to_string().parse().unwrap(),
    );
    h.insert(
        "repr-digest",
        format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(&bytes)))
            .parse()
            .unwrap(),
    );
    // Bundler versions using the original RFC 3230 spelling still verify this header.
    h.insert(
        "digest",
        format!("sha-256={}", STANDARD.encode(Sha256::digest(&bytes)))
            .parse()
            .unwrap(),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        h.insert(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{}", end - 1, bytes.len())
                .parse()
                .unwrap(),
        );
    }
    Ok(response)
}
async fn dispatch(
    state: &AppState,
    namespace: &str,
    path: &str,
    request: Request,
) -> Result<Response> {
    let read = request.method() == Method::GET || request.method() == Method::HEAD;
    let mut headers = request.headers().clone();
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
    {
        if !value.contains(' ') {
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {value}")
                    .parse()
                    .map_err(|_| Error::invalid())?,
            );
        }
    }
    let auth = authenticate(state, &headers, !read).await?;
    let owner = crate::packages::owner(state, namespace).await?;
    let token = auth.as_ref().map(|a| a.token.id.into());
    if path == "api/v1/gems" && request.method() == Method::POST {
        let auth = crate::packages::writer(auth.as_ref())?;
        let _slot = crate::packages::upload_slot(state)?;
        let bytes = to_bytes(request.into_body(), MAX_OBJECT_BYTES as usize)
            .await
            .map_err(|_| Error::invalid())?;
        let (name, key, metadata) = parse(&bytes)?;
        let package =
            crate::packages::package(state, &owner, "rubygems", &name, Some(auth), true).await?;
        let blob = codeyard_app::packages::store(
            &state.pool,
            &state.storage,
            package.id,
            auth.token.id,
            bytes.to_vec(),
            codeyard_db_repos::packages::Charge::Upload,
        )
        .await
        .map_err(Error::db)?;
        codeyard_db_repos::packages::publish(
            &state.pool,
            auth.token.id,
            Publish {
                package_id: package.id,
                name: &key,
                digest: &blob.digest,
                media_type: "application/octet-stream",
                manifest: &metadata.to_string(),
                tags: &[],
                blobs: std::slice::from_ref(&blob),
            },
        )
        .await
        .map_err(Error::db)?;
        return Ok(format!("Successfully registered gem: {name} ({key})").into_response());
    }
    if !read {
        return Err(Error(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "Unsupported RubyGems operation",
        ));
    }
    if path == "versions" {
        let mut body = String::from("---\n");
        let mut rows =
            crate::packages::owner_packages(state, &owner, "rubygems", auth.as_ref()).await?;
        if rows.is_empty() && auth.is_none() {
            return Err(Error::required());
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        for package in rows {
            let mut versions =
                crate::packages::all_versions(state, package.id, auth.as_ref()).await?;
            versions.sort_by(|a, b| a.name.cmp(&b.name));
            if versions.is_empty() {
                continue;
            }
            let info = info_body(&versions)?;
            body.push_str(&format!(
                "{} {} {}\n",
                package.name,
                versions
                    .iter()
                    .map(|v| v.name.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                md5(info.as_bytes())
            ));
        }
        return compact(body, request.method(), request.headers());
    }
    if let Some(name) = path.strip_prefix("info/") {
        let package =
            crate::packages::package(state, &owner, "rubygems", name, auth.as_ref(), false).await?;
        let mut versions = crate::packages::all_versions(state, package.id, auth.as_ref()).await?;
        versions.sort_by(|a, b| a.name.cmp(&b.name));
        return compact(info_body(&versions)?, request.method(), request.headers());
    }
    if let Some(filename) = path
        .strip_prefix("gems/")
        .and_then(|s| s.strip_suffix(".gem"))
    {
        let blob = codeyard_db_repos::packages::rubygems_file(&state.pool, &owner, token, filename)
            .await
            .map_err(Error::db)?;
        return crate::packages::oci::download(state, &blob, request.method(), request.headers())
            .await;
    }
    Err(Error::missing())
}

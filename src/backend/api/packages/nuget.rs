//! NuGet v3 discovery, publication, flat-container, registration and search.
use super::{authenticate, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        packages::{Package, Publish, Version},
    },
    packages,
    storage::MAX_OBJECT_BYTES,
};
use axum::{
    body::to_bytes,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Query, Request, State},
    http::{header, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha512};
use std::{cmp::Ordering, sync::Arc, time::Duration};

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/packages/:owner/nuget/*path", any(handle))
        .layer(DefaultBodyLimit::max(MAX_OBJECT_BYTES as usize + 65536))
}
async fn handle(
    State(state): State<Arc<AppState>>,
    Path((owner, path)): Path<(String, String)>,
    request: Request,
) -> Response {
    let anonymous = !request.headers().contains_key(header::AUTHORIZATION)
        && !request.headers().contains_key("x-nuget-apikey");
    super::finish(
        tokio::time::timeout(
            Duration::from_secs(120),
            dispatch(&state, &owner, &path, request),
        )
        .await,
        anonymous,
        super::RegistryFormat::Standard,
    )
}

fn root(owner: &str) -> String {
    format!(
        "{}/api/packages/{}/nuget",
        crate::backend::api::http::config::link_origin(),
        crate::webui::encode_component(owner)
    )
}
fn normalize(version: &str) -> Result<String> {
    if version.len() > 128 {
        return Err(Error::invalid());
    }
    let (version, build) = version
        .split_once('+')
        .map_or((version, None), |(v, b)| (v, Some(b)));
    let (core, pre) = version
        .split_once('-')
        .map_or((version, None), |(v, p)| (v, Some(p)));
    let mut parts = core
        .split('.')
        .map(|n| {
            if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::invalid());
            }
            n.parse::<u32>().map_err(|_| Error::invalid())
        })
        .collect::<Result<Vec<_>>>()?;
    if parts.is_empty() || parts.len() > 4 {
        return Err(Error::invalid());
    }
    while parts.len() < 3 {
        parts.push(0);
    }
    if parts.len() == 4 && parts[3] == 0 {
        parts.pop();
    }
    for label in [pre, build].into_iter().flatten() {
        if label
            .split('.')
            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        {
            return Err(Error::invalid());
        }
    }
    let mut normalized = parts
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".");
    if let Some(pre) = pre {
        normalized.push('-');
        normalized.push_str(&pre.to_ascii_lowercase());
    }
    Ok(normalized)
}
fn compare(a: &str, b: &str) -> Ordering {
    let (a, ap) = a.split_once('-').map_or((a, None), |(v, p)| (v, Some(p)));
    let (b, bp) = b.split_once('-').map_or((b, None), |(v, p)| (v, Some(p)));
    let core = |s: &str| {
        let mut n = [0u32; 4];
        for (i, s) in s.split('.').take(4).enumerate() {
            n[i] = s.parse().unwrap_or(0);
        }
        n
    };
    let order = core(a).cmp(&core(b));
    if order != Ordering::Equal {
        return order;
    }
    match (ap, bp) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => {
            for (a, b) in a.split('.').zip(b.split('.')) {
                let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                    (Ok(a), Ok(b)) => a.cmp(&b),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    _ => a.cmp(b),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
            a.split('.').count().cmp(&b.split('.').count())
        }
    }
}
fn metadata(version: &Version) -> Result<Value> {
    serde_json::from_str(&version.manifest).map_err(|_| Error::unavailable())
}

fn nuspec(bytes: &[u8]) -> Result<(String, String, Value)> {
    let document = super::archives::xml(bytes)?;
    if document.name != "package" {
        return Err(Error::invalid());
    }
    let spec = document.child("metadata").ok_or_else(Error::invalid)?;
    if spec.child("packageTypes").is_some_and(|types| {
        types.children.iter().any(|t| {
            t.attrs
                .get("name")
                .is_some_and(|n| n.eq_ignore_ascii_case("SymbolsPackage"))
        })
    }) {
        return Err(Error::invalid());
    }
    let id = spec.value("id");
    if !packages::valid_package_name("nuget", id) {
        return Err(Error::invalid());
    }
    let version = normalize(spec.value("version"))?;
    let dependencies = |node: &super::archives::Xml| -> Result<Vec<Value>> {
        node.children.iter().filter(|n| n.name=="dependency").map(|dep| {
            let id = dep.attrs.get("id").filter(|id| packages::valid_package_name("nuget", id)).ok_or_else(Error::invalid)?;
            Ok(json!({"id":id,"range":dep.attrs.get("version").map(String::as_str).unwrap_or("(, )")}))
        }).collect()
    };
    let mut groups = Vec::new();
    if let Some(deps) = spec.child("dependencies") {
        let ungrouped = dependencies(deps)?;
        if !ungrouped.is_empty() {
            groups.push(json!({"dependencies":ungrouped}));
        }
        for group in deps.children.iter().filter(|n| n.name == "group") {
            groups.push(json!({"targetFramework":group.attrs.get("targetFramework").map(String::as_str).unwrap_or(""),"dependencies":dependencies(group)?}));
        }
    }
    let catalog = json!({"id":id,"version":version,"title":spec.value("title"),"authors":spec.value("authors"),"description":spec.value("description"),"summary":spec.value("summary"),"projectUrl":spec.value("projectUrl"),"tags":spec.value("tags").split_whitespace().collect::<Vec<_>>(),"dependencyGroups":groups,"requireLicenseAcceptance":spec.value("requireLicenseAcceptance")=="true","listed":true});
    Ok((id.to_ascii_lowercase(), version, catalog))
}

async fn payload(request: Request) -> Result<Vec<u8>> {
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|s| s.starts_with("multipart/form-data"))
    {
        let mut multipart = Multipart::from_request(request, &())
            .await
            .map_err(|_| Error::invalid())?;
        let field = multipart
            .next_field()
            .await
            .map_err(|_| Error::invalid())?
            .ok_or_else(Error::invalid)?;
        if field
            .name()
            .is_some_and(|n| !n.eq_ignore_ascii_case("package"))
        {
            return Err(Error::invalid());
        }
        let bytes = field.bytes().await.map_err(|_| Error::invalid())?;
        if bytes.len() > MAX_OBJECT_BYTES as usize
            || multipart
                .next_field()
                .await
                .map_err(|_| Error::invalid())?
                .is_some()
        {
            return Err(Error::invalid());
        }
        Ok(bytes.to_vec())
    } else {
        Ok(to_bytes(request.into_body(), MAX_OBJECT_BYTES as usize)
            .await
            .map_err(|_| Error::invalid())?
            .to_vec())
    }
}

#[derive(Default, Deserialize)]
struct Search {
    q: Option<String>,
    skip: Option<usize>,
    take: Option<usize>,
    prerelease: Option<bool>,
    #[serde(rename = "semVerLevel")]
    semver_level: Option<String>,
}

fn supports_semver2(level: Option<&str>) -> Result<bool> {
    level.map(|level| normalize(level).map(|level| compare(&level, "2.0.0").is_ge())).transpose().map(|level| level.unwrap_or(false))
}

fn compatible_versions(versions: Vec<Version>, semver2: bool) -> Result<Vec<Version>> {
    let mut output = Vec::with_capacity(versions.len());
    for version in versions {
        if semver2 || !metadata(&version)?["semver2"].as_bool()
            .unwrap_or_else(|| crate::backend::nuget::semver2(&version.name))
        {
            output.push(version);
        }
    }
    Ok(output)
}
async fn dispatch(
    state: &AppState,
    namespace: &str,
    path: &str,
    request: Request,
) -> Result<Response> {
    let read = request.method() == Method::GET || request.method() == Method::HEAD;
    let mut headers = request.headers().clone();
    if !read {
        if let Some(key) = headers.get("x-nuget-apikey").cloned() {
            if headers.get_all("x-nuget-apikey").iter().count() != 1 {
                return Err(Error::invalid());
            }
            let key = key.to_str().map_err(|_| Error::invalid())?;
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {key}")
                    .parse()
                    .map_err(|_| Error::invalid())?,
            );
        }
    }
    let auth = authenticate(state, &headers, !read).await?;
    let _metadata_slot = if read {
        Some(state.storage.try_acquire_download().ok_or_else(Error::unavailable)?)
    } else {
        None
    };
    let owner = super::owner(state, namespace).await?;
    let base = root(namespace);
    if path == "index.json" && read {
        let resources = [
            ("PackagePublish/2.0.0", "publish"),
            ("PackageBaseAddress/3.0.0", "flatcontainer/"),
            ("RegistrationsBaseUrl/3.6.0", "registration-semver2/"),
            ("RegistrationsBaseUrl/3.0.0-rc", "registration/"),
            ("SearchQueryService/3.5.0", "query"),
            ("SearchQueryService/3.0.0-beta", "query"),
        ]
        .into_iter()
        .map(|(kind, path)| json!({"@id":format!("{base}/{path}"),"@type":kind}))
        .collect::<Vec<_>>();
        return Ok(Json(json!({"version":"3.0.0","resources":resources})).into_response());
    }
    if path == "publish" && request.method() == Method::PUT {
        let auth = super::writer(auth.as_ref())?;
        let slot = super::upload_slot(state)?;
        let bytes = payload(request).await?;
        // ZIP indexing, XML projection and whole-archive hashing are CPU/blocking
        // work. Cancellation must retain upload admission until that work ends.
        let (bytes, name, version, catalog, _slot) = tokio::task::spawn_blocking(move || {
            let spec = super::archives::nuspec(&bytes)?;
            let (name, version, mut catalog) = nuspec(&spec)?;
            catalog["packageHash"] = json!(STANDARD.encode(Sha512::digest(&bytes)));
            catalog["packageHashAlgorithm"] = json!("SHA512");
            catalog["nuspec"] = json!(String::from_utf8(spec).map_err(|_| Error::invalid())?);
            catalog["semver2"] = json!(crate::backend::nuget::metadata_semver2(&version, &catalog).map_err(Error::db)?);
            Ok::<_, Error>((bytes, name, version, catalog, slot))
        }).await.map_err(|_| Error::unavailable())??;
        let package = super::package(state, &owner, "nuget", &name, Some(auth), true).await?;
        let blob = packages::store(
            &state.pool,
            &state.storage,
            package.id,
            auth.token.id,
            bytes,
        )
        .await
        .map_err(Error::db)?;
        db::packages::publish(
            &state.pool,
            auth.token.id,
            Publish {
                package_id: package.id,
                name: &version,
                digest: &blob.digest,
                media_type: "application/zip",
                manifest: &catalog.to_string(),
                tags: &[],
                blobs: std::slice::from_ref(&blob),
            },
        )
        .await
        .map_err(Error::db)?;
        return Ok(StatusCode::CREATED.into_response());
    }
    if !read {
        return Err(Error(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "Unsupported NuGet operation",
        ));
    }
    if path == "query" {
        let Query(query) =
            Query::<Search>::try_from_uri(request.uri()).map_err(|_| Error::invalid())?;
        let semver2 = supports_semver2(query.semver_level.as_deref())?;
        let registration_kind = if semver2 { "registration-semver2" } else { "registration" };
        let needle = query.q.unwrap_or_default().to_lowercase();
        let skip = query.skip.unwrap_or(0).min(1_000_000);
        let take = query.take.unwrap_or(20).min(100);
        let mut packages = super::owner_packages(state, &owner, "nuget", auth.as_ref()).await?;
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        let mut total = 0usize;
        let mut constructed = 128usize;
        let mut data = Vec::new();
        for package in packages {
            let mut versions = versions(state, &package, auth.as_ref()).await?;
            versions = compatible_versions(versions, semver2)?;
            let downloads = versions.iter().try_fold(0u64, |n, v| {
                n.checked_add(v.downloads as u64)
                    .ok_or_else(Error::unavailable)
            })?;
            if !query.prerelease.unwrap_or(false) {
                versions.retain(|v| !v.name.contains('-'));
            }
            let Some(latest) = versions.last() else {
                continue;
            };
            let catalog = metadata(latest)?;
            if !format!(
                "{} {} {}",
                package.name, catalog["title"], catalog["description"]
            )
            .to_lowercase()
            .contains(&needle)
            {
                continue;
            }
            let selected = total >= skip && data.len() < take;
            total += 1;
            if !selected { continue; }
            // Count text, repeated URLs and JSON container overhead before constructing
            // the selected record. Unselected matches retain no response document.
            fn allocation(value: &Value) -> usize {
                128usize.saturating_add(match value {
                    Value::Array(values) => values.iter().map(allocation).fold(0usize, usize::saturating_add),
                    Value::Object(values) => values.iter().map(|(key, value)| key.len().saturating_add(allocation(value))).fold(0usize, usize::saturating_add),
                    Value::String(value) => value.len(),
                    _ => 0,
                })
            }
            let metadata_cost = catalog.to_string().len().saturating_add(allocation(&catalog))
                .saturating_add(catalog["authors"].as_str().unwrap_or("").split(',').count().saturating_mul(64));
            let cost = versions.iter().try_fold(metadata_cost, |bytes, version| {
                bytes.checked_add(version.name.len().saturating_mul(2))
                    .and_then(|bytes| bytes.checked_add(base.len() + package.name.len() + 768))
            }).ok_or_else(Error::unavailable)?;
            constructed = constructed.checked_add(cost).ok_or_else(Error::unavailable)?;
            if constructed > db::packages::MAX_METADATA_RESPONSE_BYTES { return Err(Error::unavailable()); }
            data.push(json!({"id":catalog["id"],"version":latest.name,"description":catalog["description"],"title":catalog["title"],"authors":catalog["authors"].as_str().unwrap_or("").split(',').map(str::trim).collect::<Vec<_>>(),"tags":catalog["tags"],"totalDownloads":downloads,"registration":format!("{base}/{registration_kind}/{}/index.json",package.name),"versions":versions.iter().map(|v| json!({"version":v.name,"downloads":v.downloads,"@id":format!("{base}/{registration_kind}/{}/{}.json",package.name,v.name)})).collect::<Vec<_>>()}));
        }
        let bytes = serde_json::to_vec(&json!({"totalHits":total,"data":data})).map_err(|_| Error::unavailable())?;
        if bytes.len() > db::packages::MAX_METADATA_RESPONSE_BYTES { return Err(Error::unavailable()); }
        return Ok(([(header::CONTENT_TYPE, "application/json")], bytes).into_response());
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() < 3 {
        return Err(Error::missing());
    }
    let id = parts[1].to_ascii_lowercase();
    let package = super::package(state, &owner, "nuget", &id, auth.as_ref(), false).await?;
    let token = auth.as_ref().map(|a| a.token.id.into());
    if parts[0] == "flatcontainer" {
        if parts.len() == 3 && parts[2] == "index.json" {
            let mut versions = super::all_version_fields(state, package.id, auth.as_ref(), true).await?;
            versions.sort_by(|a, b| compare(&a.name, &b.name));
            return Ok(Json(
                json!({"versions":versions.iter().map(|v| &v.name).collect::<Vec<_>>()}),
            )
            .into_response());
        }
        if parts.len() != 4 {
            return Err(Error::missing());
        }
        let key = normalize(parts[2])?;
        let version = db::packages::version(&state.pool, package.id, token, &key)
            .await
            .map_err(Error::db)?;
        let catalog = metadata(&version)?;
        if parts[3] == format!("{id}.nuspec") {
            return Ok((
                [
                    (header::CONTENT_TYPE, "application/xml"),
                    (header::CONTENT_SECURITY_POLICY, "default-src 'none'; sandbox"),
                ],
                catalog["nuspec"]
                    .as_str()
                    .ok_or_else(Error::unavailable)?
                    .to_owned(),
            )
                .into_response());
        }
        if parts[3] == format!("{id}.{key}.nupkg.sha512") {
            return Ok(catalog["packageHash"]
                .as_str()
                .ok_or_else(Error::unavailable)?
                .to_owned()
                .into_response());
        }
        if parts[3] != format!("{id}.{key}.nupkg") {
            return Err(Error::missing());
        }
        let blob = db::packages::blob(&state.pool, package.id, token, &version.digest)
            .await
            .map_err(Error::db)?;
        let response =
            super::oci::download(state, &blob, request.method(), request.headers()).await?;
        if request.method() == Method::GET {
            db::packages::nuget_downloaded(&state.pool, package.id, token, version.id)
                .await
                .map_err(Error::db)?;
        }
        return Ok(response);
    }
    if matches!(parts[0], "registration" | "registration-semver2") && parts.len() == 3 {
        let registration_kind = parts[0];
        let versions = compatible_versions(versions(state, &package, auth.as_ref()).await?, registration_kind == "registration-semver2")?;
        if versions.is_empty() {
            return Err(Error::missing());
        }
        let items = versions
            .iter()
            .map(|v| registration(&base, registration_kind, &package, v))
            .collect::<Result<Vec<_>>>()?;
        let page = json!({"@id":format!("{base}/{registration_kind}/{id}/page.json"),"count":items.len(),"lower":versions.first().map(|v| &v.name),"upper":versions.last().map(|v| &v.name),"items":items});
        if parts[2] == "index.json" {
            return Ok(Json(json!({"@id":format!("{base}/{registration_kind}/{id}/index.json"),"count":1,"items":[page]})).into_response());
        }
        if parts[2] == "page.json" {
            return Ok(Json(page).into_response());
        }
        if let Some(key) = parts[2].strip_suffix(".json") {
            let version = versions
                .iter()
                .find(|v| v.name == key)
                .ok_or_else(Error::missing)?;
            let mut leaf = registration(&base, registration_kind, &package, version)?;
            leaf["catalogEntry"] = json!(format!("{base}/catalog/{id}/{key}.json"));
            return Ok(Json(leaf).into_response());
        }
    }
    if parts[0] == "catalog" && parts.len() == 3 {
        let key = parts[2].strip_suffix(".json").ok_or_else(Error::missing)?;
        let version = db::packages::version(&state.pool, package.id, token, key)
            .await
            .map_err(Error::db)?;
        return Ok(
            Json(registration(&base, "registration-semver2", &package, &version)?["catalogEntry"].clone()).into_response(),
        );
    }
    Err(Error::missing())
}
async fn versions(
    state: &AppState,
    package: &Package,
    auth: Option<&super::Authentication>,
) -> Result<Vec<Version>> {
    let mut versions = super::all_versions(state, package.id, auth).await?;
    versions.sort_by(|a, b| compare(&a.name, &b.name));
    Ok(versions)
}
fn registration(base: &str, registration_kind: &str, package: &Package, version: &Version) -> Result<Value> {
    let mut catalog = metadata(version)?;
    let object = catalog.as_object_mut().ok_or_else(Error::unavailable)?;
    object.remove("nuspec");
    object.remove("semver2");
    let published = chrono::DateTime::from_timestamp(version.created_at, 0)
        .ok_or_else(Error::unavailable)?
        .to_rfc3339();
    catalog["published"] = json!(published);
    catalog["@id"] = json!(format!(
        "{base}/catalog/{}/{}.json",
        package.name, version.name
    ));
    Ok(
        json!({"@id":format!("{base}/{registration_kind}/{}/{}.json",package.name,version.name),"catalogEntry":catalog,"listed":true,"published":published,"packageContent":format!("{base}/flatcontainer/{}/{}/{}.{}.nupkg",package.name,version.name,package.name,version.name),"registration":format!("{base}/{registration_kind}/{}/index.json",package.name)}),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn versions_have_nuget_identity_and_ordering() {
        assert!(crate::backend::packages::valid_package_name(
            "nuget",
            "Example.Library-Tools"
        ));
        for id in ["-options", "../escape", "Bad..Id", "Bad--Id"] {
            assert!(!crate::backend::packages::valid_package_name("nuget", id));
        }
        assert_eq!(
            super::normalize("01.2.0.0-RC.2+build").unwrap(),
            "1.2.0-rc.2"
        );
        assert!(super::normalize("1.0/evil").is_err());
        assert!(super::compare("1.10.0", "1.2.0").is_gt());
        assert!(super::compare("1.0.0", "1.0.0-rc.2").is_gt());
        assert!(super::compare("1.0.0-rc.10", "1.0.0-rc.2").is_gt());
    }
}

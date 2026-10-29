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
            ("RegistrationsBaseUrl/3.6.0", "registration/"),
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
        let _slot = super::upload_slot(state)?;
        let bytes = payload(request).await?;
        let spec = super::archives::nuspec(&bytes)?;
        let (name, version, mut catalog) = nuspec(&spec)?;
        let package = super::package(state, &owner, "nuget", &name, Some(auth), true).await?;
        let blob = packages::store(
            &state.pool,
            &state.storage,
            package.id,
            auth.token.id,
            bytes.clone(),
        )
        .await
        .map_err(Error::db)?;
        catalog["packageHash"] = json!(STANDARD.encode(Sha512::digest(&bytes)));
        catalog["packageHashAlgorithm"] = json!("SHA512");
        catalog["nuspec"] = json!(String::from_utf8(spec).map_err(|_| Error::invalid())?);
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
        let needle = query.q.unwrap_or_default().to_lowercase();
        let mut data = Vec::new();
        for package in super::owner_packages(state, &owner, "nuget", auth.as_ref()).await? {
            let mut versions = versions(state, &package, auth.as_ref()).await?;
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
            data.push(json!({"id":catalog["id"],"version":latest.name,"description":catalog["description"],"title":catalog["title"],"authors":catalog["authors"].as_str().unwrap_or("").split(',').map(str::trim).collect::<Vec<_>>(),"tags":catalog["tags"],"totalDownloads":downloads,"registration":format!("{base}/registration/{}/index.json",package.name),"versions":versions.iter().map(|v| json!({"version":v.name,"downloads":v.downloads,"@id":format!("{base}/registration/{}/{}.json",package.name,v.name)})).collect::<Vec<_>>()}));
        }
        data.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        let total = data.len();
        let data: Vec<_> = data
            .into_iter()
            .skip(query.skip.unwrap_or(0).min(1_000_000))
            .take(query.take.unwrap_or(20).min(100))
            .collect();
        return Ok(Json(json!({"totalHits":total,"data":data})).into_response());
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
    if parts[0] == "registration" && parts.len() == 3 {
        let versions = versions(state, &package, auth.as_ref()).await?;
        if versions.is_empty() {
            return Err(Error::missing());
        }
        let items = versions
            .iter()
            .map(|v| registration(&base, &package, v))
            .collect::<Result<Vec<_>>>()?;
        let page = json!({"@id":format!("{base}/registration/{id}/page.json"),"count":items.len(),"lower":versions.first().map(|v| &v.name),"upper":versions.last().map(|v| &v.name),"items":items});
        if parts[2] == "index.json" {
            return Ok(Json(json!({"@id":format!("{base}/registration/{id}/index.json"),"count":1,"items":[page]})).into_response());
        }
        if parts[2] == "page.json" {
            return Ok(Json(page).into_response());
        }
        if let Some(key) = parts[2].strip_suffix(".json") {
            let version = versions
                .iter()
                .find(|v| v.name == key)
                .ok_or_else(Error::missing)?;
            let mut leaf = registration(&base, &package, version)?;
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
            Json(registration(&base, &package, &version)?["catalogEntry"].clone()).into_response(),
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
fn registration(base: &str, package: &Package, version: &Version) -> Result<Value> {
    let mut catalog = metadata(version)?;
    catalog
        .as_object_mut()
        .ok_or_else(Error::unavailable)?
        .remove("nuspec");
    let published = chrono::DateTime::from_timestamp(version.created_at, 0)
        .ok_or_else(Error::unavailable)?
        .to_rfc3339();
    catalog["published"] = json!(published);
    catalog["@id"] = json!(format!(
        "{base}/catalog/{}/{}.json",
        package.name, version.name
    ));
    Ok(
        json!({"@id":format!("{base}/registration/{}/{}.json",package.name,version.name),"catalogEntry":catalog,"listed":true,"published":published,"packageContent":format!("{base}/flatcontainer/{}/{}/{}.{}.nupkg",package.name,version.name,package.name,version.name),"registration":format!("{base}/registration/{}/index.json",package.name)}),
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

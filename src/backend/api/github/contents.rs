//! Contents API preconditions use blob IDs; publication still leases the entire branch tip.
use super::{auth, repo_url, repository, Error, Result};
use crate::backend::{
    app::AppState,
    repositories::{
        files,
        store::{repo_store, ErrorKind, FileWrite, RepoError, RepoErrorReason, MAX_EDITOR_BYTES},
    },
};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

pub(super) use crate::webui::pages::repos::ref_path::encode_path as path_url;

pub(super) fn store_error(error: RepoError) -> Error {
    if error.is_write_denied() || error.is_protected_branch() {
        return Error::new(StatusCode::FORBIDDEN, "Repository write denied");
    }
    if error.is_response_too_large() {
        return Error::new(
            StatusCode::FORBIDDEN,
            "Object exceeds the response size limit",
        );
    }
    match error.kind {
        ErrorKind::NotFound | ErrorKind::MissingPath => Error::missing(),
        ErrorKind::Conflict => Error::new(StatusCode::CONFLICT, "Reference changed"),
        ErrorKind::Invalid => Error::invalid("Invalid repository operation"),
        ErrorKind::Unavailable => Error::unavailable(()),
    }
}

fn file_error(error: files::WriteError) -> Error {
    use files::WriteFailure;
    let status = match error.kind {
        WriteFailure::Invalid | WriteFailure::Encoding => StatusCode::UNPROCESSABLE_ENTITY,
        WriteFailure::Conflict => StatusCode::CONFLICT,
        WriteFailure::Unavailable | WriteFailure::SavedFollowupFailed => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        WriteFailure::NotFound => StatusCode::NOT_FOUND,
        WriteFailure::Denied => StatusCode::FORBIDDEN,
        WriteFailure::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
    };
    Error::new(status, error.message)
}

const SYMLINK_MODE: u32 = 0o120000;

/// The repository path a relative link at `path` points to, if it stays inside the repository.
fn link_target(path: &str, target: &str) -> Option<String> {
    if target.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The URL `.gitmodules` records for the submodule at `path`.
fn submodule_url(modules: &str, path: &str) -> Option<String> {
    let (mut module_path, mut url) = (None, None);
    for line in modules.lines().map(str::trim).chain(["["]) {
        if line.starts_with('[') {
            if module_path.as_deref() == Some(path) {
                return url;
            }
            (module_path, url) = (None, None);
        } else if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "path" => module_path = Some(value.trim().to_owned()),
                "url" => url = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    None
}

pub(super) fn entry(
    owner: &str,
    repo: &str,
    reference: &str,
    path: &str,
    sha: &str,
    size: u64,
    kind: &str,
) -> Value {
    let url = format!(
        "{}/contents/{}?ref={}",
        repo_url(owner, repo),
        path_url(path),
        crate::webui::encode_component(reference)
    );
    let base = format!("{}/{}/{}", crate::backend::api::http::config::link_origin(), crate::webui::encode_component(owner), crate::webui::encode_component(repo));
    let html = crate::webui::pages::repos::ref_path::file_url(&base, if kind == "dir" { "tree" } else { "blob" }, reference, path);
    let git_url = format!(
        "{}/git/{}/{sha}",
        repo_url(owner, repo),
        if kind == "dir" { "trees" } else { "blobs" }
    );
    // REST clients can follow this URL with the same bearer credential. Native raw URLs
    // remain available for browser-session downloads.
    let download = (kind == "file").then(|| format!("{url}&raw=1"));
    json!({
        "type": kind, "name": path.rsplit('/').next().unwrap_or(path), "path": path,
        "sha": sha, "size": size, "url": url, "html_url": html, "git_url": git_url,
        "download_url": download, "_links": {"self": url, "git": git_url, "html": html},
    })
}

pub(super) use crate::backend::webhooks::payloads::git_commit as commit_json;

pub(super) async fn blob_response(
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    reference: &str,
    path: &str,
    blob: crate::backend::repositories::store::BlobBytes,
) -> Result<Response> {
    if headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/vnd.github.raw"))
    {
        return Ok((
            [(header::CONTENT_TYPE, "application/octet-stream")],
            blob.bytes,
        ).into_response());
    }
    if headers.get(header::ACCEPT).and_then(|value| value.to_str().ok()).is_some_and(|value| value.contains("application/vnd.github.html")) {
        let text = String::from_utf8(blob.bytes).map_err(|_| Error::invalid("HTML representation requires a text file"))?;
        let references = crate::webui::components::references::References { owner: owner.to_owned(), repo: name.to_owned(), ..Default::default() };
        return super::meta::render(text, references).await;
    }
    let mut value = entry(owner, name, reference, path, &blob.id, blob.size, "file");
    value["encoding"] = json!("base64");
    value["content"] = json!(STANDARD.encode(&blob.bytes));
    Ok(Json(value).into_response())
}

async fn readme(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response> {
    let (owner, name) = (&params["owner"], &params["repo"]);
    let (repo, _) = repository(&state, &headers, owner, name, false).await?;
    let reference = query.get("ref").unwrap_or(&repo.default_branch);
    let directory = params.get("dir").map(String::as_str).unwrap_or("");
    let store = repo_store(&repo, state.cluster.as_ref());
    let revision = store.commit(reference).await.map_err(store_error)?.sha;
    let mut entries = store
        .tree_metadata(&revision, directory)
        .await
        .map_err(store_error)?;
    entries.sort_by_key(|entry| entry.name.to_ascii_lowercase());
    let path = entries
        .into_iter()
        .find(|entry| {
            let name = entry.name.to_ascii_lowercase();
            entry.entry_type == "file" && (name == "readme" || name.starts_with("readme."))
        })
        .ok_or_else(Error::missing)?
        .path;
    let blob = store
        .blob_bytes(&revision, &path)
        .await
        .map_err(store_error)?;
    auth::recheck_repository_read(&state, &headers, repo.id, "contents").await?;
    blob_response(&headers, owner, name, &revision, &path, blob).await
}

async fn read(
    State(state): State<Arc<AppState>>,
    mut headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response> {
    let (owner, name) = (&params["owner"], &params["repo"]);
    let (repo, _) = repository(&state, &headers, owner, name, false).await?;
    let reference = query.get("ref").unwrap_or(&repo.default_branch);
    let response = async {
    let path = params.get("path").map(String::as_str).unwrap_or("");
    if query.get("raw").is_some_and(|value| value == "1") {
        headers.insert(header::ACCEPT, axum::http::HeaderValue::from_static("application/vnd.github.raw"));
    }
    let store = repo_store(&repo, state.cluster.as_ref());
    // Pin both file and directory reads so a moving branch cannot mix revisions.
    let revision = store.commit(reference).await.map_err(store_error)?.sha;
    if !path.is_empty() {
        match store.blob_bytes(&revision, path).await {
            Ok(blob) if blob.mode == SYMLINK_MODE => {
                // A link to a file in the repository answers with that file, as on GitHub.
                // Any other link describes itself.
                let target = String::from_utf8_lossy(&blob.bytes).into_owned();
                if let Some(resolved) = link_target(path, &target) {
                    match store.blob_bytes(&revision, &resolved).await {
                        Ok(file) if file.mode != SYMLINK_MODE => {
                            return blob_response(
                                &headers, owner, name, &revision, &resolved, file,
                            ).await;
                        }
                        Ok(_) => {}
                        Err(error)
                            if error.kind == ErrorKind::MissingPath
                                || error.reason == Some(RepoErrorReason::NotFile) => {}
                        Err(error) => return Err(store_error(error)),
                    }
                }
                let mut value = entry(owner, name, &revision, path, &blob.id, blob.size, "symlink");
                value["target"] = json!(target);
                return Ok(Json(value).into_response());
            }
            Ok(blob) => {
                return blob_response(&headers, owner, name, &revision, path, blob).await;
            }
            Err(error) if error.reason == Some(RepoErrorReason::NotFile) => {
                // A submodule is a commit entry, not a directory to list.
                let (parent, leaf) = path.rsplit_once('/').unwrap_or(("", path));
                let entries = store
                    .tree_metadata(&revision, parent)
                    .await
                    .map_err(store_error)?;
                if let Some(link) = entries
                    .iter()
                    .find(|item| item.name == leaf && item.entry_type == "gitlink")
                {
                    let url = match store.blob_bytes(&revision, ".gitmodules").await {
                        Ok(modules) => {
                            submodule_url(&String::from_utf8_lossy(&modules.bytes), path)
                        }
                        Err(error) if error.kind == ErrorKind::MissingPath => None,
                        Err(error) => return Err(store_error(error)),
                    };
                    let mut value = entry(owner, name, &revision, path, &link.sha, 0, "submodule");
                    value["submodule_git_url"] = json!(url);
                    value["git_url"] = Value::Null;
                    value["html_url"] = Value::Null;
                    value["download_url"] = Value::Null;
                    value["_links"]["git"] = Value::Null;
                    value["_links"]["html"] = Value::Null;
                    return Ok(Json(value).into_response());
                }
            }
            Err(error) => return Err(store_error(error)),
        }
    }
    let entries = store
        .tree_metadata(&revision, path)
        .await
        .map_err(store_error)?;
    let entries = entries
            .into_iter()
            .map(|item| {
                entry(
                    owner,
                    name,
                    &revision,
                    &item.path,
                    &item.sha,
                    item.size.unwrap_or(0),
                    if item.entry_type == "dir" {
                        "dir"
                    } else {
                        "file"
                    },
                )
            })
            .collect::<Vec<_>>();
    let value = if headers.get(header::ACCEPT).and_then(|value| value.to_str().ok()).is_some_and(|value| value.contains("application/vnd.github.object")) { json!({"entries": entries}) } else { json!(entries) };
    Ok(Json(value).into_response())
    }.await?;
    auth::recheck_repository_read(&state, &headers, repo.id, "contents").await?;
    Ok(response)
}

#[derive(Deserialize)]
struct Write {
    #[serde(flatten)]
    identities: crate::backend::repositories::store::CommitIdentities,
    message: String,
    content: Option<String>,
    sha: Option<String>,
    branch: Option<String>,
}

async fn save(
    state: Arc<AppState>,
    headers: HeaderMap,
    params: HashMap<String, String>,
    input: Write,
    deleting: bool,
) -> Result<Response> {
    let (owner, name, path) = (&params["owner"], &params["repo"], &params["path"]);
    let (repo, auth) = repository(&state, &headers, owner, name, true).await?;
    let auth = auth.ok_or_else(auth::required)?;
    let authority = auth.database_actor();
    let branch = input.branch.as_deref().unwrap_or(&repo.default_branch);
    let store = repo_store(&repo, state.cluster.as_ref());
    let refs = store.refs_from_primary().await.map_err(store_error)?;
    let expected = refs.get(&format!("refs/heads/{branch}"));
    // Only an empty repository starts a new history; a misspelled branch elsewhere is missing.
    if expected.is_none() && refs.keys().any(|name| name.starts_with("refs/heads/")) {
        return Err(Error::missing());
    }
    let blob = match expected {
        Some(revision) => match store.blob_range(revision, path, crate::backend::repositories::store::BlobRange::Window { offset: 0, length: 0 }).await {
            Ok(blob) => Some(blob),
            Err(error) if error.kind == ErrorKind::MissingPath => None,
            Err(error) => return Err(store_error(error)),
        },
        None => None,
    };
    let creating = blob.is_none();
    match (&blob, &input.sha) {
        (Some(blob), Some(sha)) if blob.id == *sha => {}
        (None, None) if !deleting => {}
        (Some(_), None) => return Err(Error::invalid("sha is required when updating a file")),
        (None, _) if deleting => return Err(Error::missing()),
        _ => return Err(Error::new(StatusCode::CONFLICT, "File SHA does not match")),
    }
    let bytes = if deleting {
        Vec::new()
    } else {
        let content = input
            .content
            .as_deref()
            .ok_or_else(|| Error::invalid("content is required"))?;
        files::decode_base64(content, MAX_EDITOR_BYTES).map_err(|error| match error {
            files::WriteFailure::TooLarge => Error::new(StatusCode::PAYLOAD_TOO_LARGE, "File exceeds 1 MiB"),
            _ => Error::invalid("Invalid base64 content"),
        })?
    };
    let write = FileWrite {
        branch: branch.to_owned(),
        path: path.to_owned(),
        message: input.message,
        expected_revision: expected.cloned().unwrap_or_else(|| "unborn".into()),
        creating,
        deleting,
    };
    let saved = if deleting {
        files::delete_file(
            &state.pool,
            state.cluster.as_ref(),
            &state.ci_runs,
            authority,
            owner,
            repo.id,
            write,
            None,
            input.identities,
        )
        .await
    } else {
        files::write_file_bytes(
            &state.pool,
            state.cluster.as_ref(),
            &state.ci_runs,
            authority,
            owner,
            repo.id,
            write,
            bytes,
            input.identities,
        )
        .await
    }
    .map_err(file_error)?;
    let content = if deleting {
        Value::Null
    } else {
        let blob = store
            .blob_bytes(&saved.commit.sha, path)
            .await
            .map_err(store_error)?;
        entry(owner, name, branch, path, &blob.id, blob.size, "file")
    };
    let status = if creating {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(json!({"content": content, "commit": commit_json(&saved.commit, owner, name, repo.id)})),
    )
        .into_response())
}

async fn put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Json(input): Json<Write>,
) -> Result<Response> {
    save(state, headers, params, input, false).await
}

async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Json(input): Json<Write>,
) -> Result<Response> {
    save(state, headers, params, input, true).await
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/get-content", get(read)),
        ("repos/get-readme", get(readme)),
        ("repos/get-readme-in-directory", get(readme)),
        (
            "repos/create-or-update-file-contents",
            axum::routing::put(put),
        ),
        ("repos/delete-file", axum::routing::delete(delete)),
    ]
}

/// The repository root, which GitHub serves as `contents` with no path.
pub(super) fn aliases() -> Router<Arc<AppState>> {
    Router::new().route("/repos/:owner/:repo/contents", get(read))
}

#[cfg(test)]
mod link_tests {
    #[test]
    fn links_and_submodules_resolve_inside_the_repository() {
        assert_eq!(
            super::link_target("docs/link", "../README.md").as_deref(),
            Some("README.md")
        );
        assert_eq!(super::link_target("link", "../outside"), None);
        assert_eq!(super::link_target("link", "/etc/passwd"), None);
        let modules =
            "[submodule \"lib\"]\n\tpath = vendor/lib\n\turl = https://example.test/lib.git\n";
        assert_eq!(
            super::submodule_url(modules, "vendor/lib").as_deref(),
            Some("https://example.test/lib.git")
        );
        assert_eq!(super::submodule_url(modules, "other"), None);
    }
}

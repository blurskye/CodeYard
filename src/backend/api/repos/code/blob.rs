use super::tree::view_entries;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{
    is_branch, load_refs_from_repo, number, ref_names_under, resolve, root_if_missing, store_error,
    Loaded,
};
use crate::backend::api::repos::refs::references::resolve_references;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::git::MAX_BLAME_FILE_BYTES;
use crate::backend::repositories::files;
use crate::backend::repositories::store::{
    repo_store, BlobBytes, BlobRange, RepoError, RepoStore, MAX_EDITOR_BYTES,
};
use crate::webui::pages::repos as view;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize)]
pub struct BlobResponse {
    pub path: String,
    pub content: String,
    pub encoding: String,
    pub size: u64,
    pub is_binary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_offset: Option<u64>,
    pub truncated: bool,
}

#[derive(Deserialize)]
pub struct BlobQuery {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

/// The smallest text window. After a split character loses up to three bytes, five
/// bytes remain, which always hold one whole character, so every page makes progress.
const MIN_TEXT_WINDOW: usize = 8;

/// The whole characters of a text window. `start` and `end` are the byte offsets
/// where `text` begins and ends, so the next window can begin exactly at `end`.
struct TextWindow {
    start: u64,
    end: u64,
    text: String,
}

/// Cuts a text window back to whole UTF-8 characters. A window that starts inside a
/// character drops the rest of it. One that ends inside a character stops before it,
/// unless the window reaches the end of the blob.
fn text_window(offset: u64, bytes: &[u8], size: u64) -> TextWindow {
    let reaches_end = offset + bytes.len() as u64 == size;
    let mut skip = 0;
    if offset > 0 {
        while skip < bytes.len().min(3) && (bytes[skip] & 0xC0) == 0x80 {
            skip += 1;
        }
    }
    let mut end = bytes.len();
    if !reaches_end {
        // Only one of the last three bytes can start a character that is cut off.
        let lead = (skip.max(end.saturating_sub(3))..end)
            .rev()
            .find(|&index| (bytes[index] & 0xC0) != 0x80);
        if let Some(lead) = lead {
            let width = match bytes[lead] {
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => 1,
            };
            if lead + width > end {
                end = lead;
            }
        }
    }
    let kept = &bytes[skip..end];
    let start = offset + skip as u64;
    TextWindow {
        start,
        end: start + kept.len() as u64,
        text: String::from_utf8_lossy(kept).into_owned(),
    }
}

pub async fn get_blob(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, git_ref, path)): Path<(String, String, String, String)>,
    Query(query): Query<BlobQuery>,
    headers: HeaderMap,
) -> Result<Json<BlobResponse>, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    "database_error",
                    "Repository operation is unavailable",
                )),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    check_repo_read_access(&state.pool, &repository, &headers).await?;

    let refs = load_refs_from_repo(&state, &repository, &format!("{}/{}", owner, repo)).await?;
    if refs.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Repository is empty")),
        ));
    }

    const DEFAULT_PREVIEW_LIMIT: usize = 256 * 1024;
    const MAX_PREVIEW_LIMIT: usize = 2 * 1024 * 1024;
    const MAX_INLINE_BINARY_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;

    let preview_limit = query
        .limit
        .unwrap_or(DEFAULT_PREVIEW_LIMIT)
        .clamp(MIN_TEXT_WINDOW, MAX_PREVIEW_LIMIT);
    let store = repo_store(&repository, state.cluster.as_ref());
    let preview = store
        .blob_range(
            &git_ref,
            &path,
            BlobRange::Window {
                offset: query.offset.unwrap_or(0) as u64,
                length: preview_limit as u64,
            },
        )
        .await
        .map_err(|e| {
            crate::backend::api::repos::responses::repo_store_error(
                e,
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?;
    let total_size = preview.size;
    let preview_offset = preview.offset;
    let preview_end = preview_offset + preview.bytes.len() as u64;
    let is_binary = preview.is_binary;
    let blob_id = preview.id.clone();

    if is_binary && total_size > MAX_INLINE_BINARY_PREVIEW_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiError::new(
                "preview_unavailable",
                "Preview unavailable for binary files larger than 8 MiB. Use the Raw view to download the file.",
            )),
        ));
    }

    let (content, encoding, truncated, next_offset, prev_offset, response_offset, response_limit) =
        if is_binary {
            let bytes = if preview_offset == 0 && preview_end == total_size {
                preview.bytes
            } else {
                store.blob_by_id(&blob_id).await.map_err(|e| {
                    crate::backend::api::repos::responses::repo_store_error(
                        e,
                        StatusCode::INTERNAL_SERVER_ERROR,
                    )
                })?
            };
            (
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
                "base64".to_string(),
                false,
                None,
                None,
                None,
                None,
            )
        } else {
            let window = text_window(preview_offset, &preview.bytes, total_size);
            let truncated = window.start > 0 || window.end < total_size;
            let next_offset = (window.end < total_size).then_some(window.end);
            let prev_offset =
                (window.start > 0).then_some(window.start.saturating_sub(preview_limit as u64));

            (
                window.text,
                "utf-8".to_string(),
                truncated,
                next_offset,
                prev_offset,
                Some(window.start),
                Some(preview_limit as u64),
            )
        };

    Ok(Json(BlobResponse {
        path,
        content,
        encoding,
        size: total_size,
        is_binary,
        preview_offset: response_offset,
        preview_limit: response_limit,
        next_offset,
        prev_offset,
        truncated,
    }))
}

async fn blob_page(ctx: &PageContext, loaded: &Loaded, tail: &str) -> Result<Response, Response> {
    let resolved = resolve(ctx, loaded, tail, true)?;
    if loaded.refs.is_empty() {
        return Err(ctx.error(StatusCode::NOT_FOUND, "Repository is empty."));
    }
    let offset = number(ctx, "offset", 0, usize::MAX)?;
    let store = repo_store(&loaded.repository, ctx.state.cluster.as_ref());
    // Resolve the ref once. The content, the Files panel and the permalink then name the same
    // commit, even if a push moves the branch while this page loads.
    let head = store
        .commit(&resolved.reference)
        .await
        .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?;
    let read = store
        .blob_range(
            &head.sha,
            &resolved.path,
            BlobRange::Window {
                offset: offset as u64,
                length: 256 * 1024,
            },
        )
        .await;
    let BlobBytes {
        id,
        bytes,
        is_binary: binary,
        size,
        offset: start,
    } = match read {
        Ok(blob) => blob,
        Err(error) => {
            return match root_if_missing(ctx, loaded, &resolved.reference, &error) {
                Some(redirect) => Ok(redirect),
                None => Err(store_error(ctx, Some(&loaded.chrome), error)),
            };
        }
    };
    let (start, end, content) = if binary {
        (start, start + bytes.len() as u64, String::new())
    } else {
        let window = text_window(start, &bytes, size);
        (window.start, window.end, window.text)
    };
    let editable = if !binary && size <= MAX_EDITOR_BYTES as u64 {
        if start == 0 && end == size {
            files::editor_text(&bytes, false).is_ok()
        } else {
            store
                .blob_by_id(&id)
                .await
                .is_ok_and(|bytes| files::editor_text(&bytes, binary).is_ok())
        }
    } else {
        false
    };
    let on_branch = is_branch(loaded, &resolved.reference);
    let mut page = view::blob::BlobPage {
        reference: resolved.reference.clone(),
        path: resolved.path,
        content,
        binary,
        size,
        offset: start,
        next: if end < size { Some(end) } else { None },
        previous: if start > 0 {
            Some(start.saturating_sub(256 * 1024))
        } else {
            None
        },
        truncated: !binary && (start > 0 || end < size),
        can_blame: !binary && size <= MAX_BLAME_FILE_BYTES as u64,
        editable: editable && on_branch,
        deletable: on_branch,
        code_view: ctx.query.get("view").is_some_and(|view| view == "code"),
        references: Default::default(),
        branches: ref_names_under(loaded, "refs/heads/"),
        tags: ref_names_under(loaded, "refs/tags/"),
        permalink: String::new(),
        folders: Vec::new(),
        latest_commit: None,
    };
    let (folders, latest_commit, permalink) =
        file_context(ctx, loaded, &store, &head.sha, &page.path).await?;
    page.folders = folders;
    page.latest_commit = latest_commit;
    page.permalink = permalink;
    if page.renders_markdown() {
        page.references = resolve_references(
            ctx,
            &loaded.repository,
            &loaded.chrome.owner,
            &loaded.chrome.name,
            &[page.content.as_str()],
            // A repository file renders as a document.
            true,
        )
        .await?;
    }
    let title = page.path.clone();
    Ok(ctx.page_model(
        &title,
        crate::webui::app::Page::Repositories(view::page::Page::Blob {
            repo: loaded.chrome.clone(),
            page,
        }),
    ))
}

/// The root and each folder on the way to `directory`, listed at `reference`, root first. The
/// directory itself comes last. `directory` is empty for the root.
pub(super) async fn folder_listings(
    store: &RepoStore,
    reference: &str,
    directory: &str,
) -> Result<Vec<view::blob::FolderListing>, RepoError> {
    let mut paths = vec![String::new()];
    let mut current = String::new();
    for part in directory.split('/').filter(|part| !part.is_empty()) {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(part);
        paths.push(current.clone());
    }
    Ok(store
        .sidebar_trees(reference, &paths)
        .await?
        .into_iter()
        .map(|(path, entries)| view::blob::FolderListing {
            path,
            entries: view_entries(entries),
        })
        .collect())
}

/// The Files panel's folders for a file, the last commit that changed it, and its permalink. The
/// file view and the blame view both show them. `commit` is the commit the file was read at, and
/// the permalink names it, so the link keeps showing the same file.
pub(super) async fn file_context(
    ctx: &PageContext,
    loaded: &Loaded,
    store: &RepoStore,
    commit: &str,
    path: &str,
) -> Result<
    (
        Vec<view::blob::FolderListing>,
        Option<view::commits::Commit>,
        String,
    ),
    Response,
> {
    let directory = path.rsplit_once('/').map_or("", |(parent, _)| parent);
    let folders = folder_listings(store, commit, directory)
        .await
        .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?;
    let latest_commit = match store
        .path_commits(commit, path, 0, 1, &Default::default())
        .await
    {
        Ok(commits) => commits
            .into_iter()
            .next()
            .map(crate::backend::api::repos::chrome::commit),
        Err(error) if error.is_response_too_large() => None,
        Err(error) => return Err(store_error(ctx, Some(&loaded.chrome), error)),
    };
    let permalink = format!(
        "{}{}",
        crate::backend::api::http::config::public_origin(),
        view::ref_path::file_url(&loaded.chrome.base(), "blob", commit, path)
    );
    Ok((folders, latest_commit, permalink))
}

pub(in crate::backend::api::repos) async fn page(
    axum::extract::State(state): axum::extract::State<
        std::sync::Arc<crate::backend::app::AppState>,
    >,
    headers: axum::http::HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    axum::extract::Path(params): axum::extract::Path<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let ctx =
        match crate::backend::api::http::context::PageContext::load(state, headers, &uri).await {
            Ok(ctx) => ctx,
            Err(response) => return response,
        };
    let loaded =
        match crate::backend::api::repos::chrome::load(&ctx, &params["owner"], &params["repo"])
            .await
        {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    let tail = params.get("ref_path").map(String::as_str).unwrap_or("");
    match blob_page(&ctx, &loaded, tail).await {
        Ok(response) | Err(response) => response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chained_text_windows_reproduce_the_text_without_splitting_characters() {
        let text = "a\u{e9}\u{20ac}\u{1f600} text\n".repeat(4);
        let bytes = text.as_bytes();
        let size = bytes.len() as u64;
        let mut offset = 0;
        let mut collected = String::new();
        while offset < size {
            let end = (offset as usize + MIN_TEXT_WINDOW).min(bytes.len());
            let window = text_window(offset, &bytes[offset as usize..end], size);
            assert!(window.end > offset, "every window must make progress");
            collected.push_str(&window.text);
            offset = window.end;
        }
        assert_eq!(collected, text);
    }

    #[test]
    fn window_starting_inside_a_character_begins_at_the_next_one() {
        let bytes = "a\u{e9}\u{20ac}".as_bytes();
        let window = text_window(2, &bytes[2..], bytes.len() as u64);
        assert_eq!((window.start, window.end), (3, 6));
        assert_eq!(window.text, "\u{20ac}");
    }

    #[test]
    fn window_ending_inside_a_character_stops_before_it() {
        let bytes = "a\u{e9}".as_bytes();
        let size = bytes.len() as u64;
        let window = text_window(0, &bytes[..2], size);
        assert_eq!((window.start, window.end), (0, 1));
        assert_eq!(window.text, "a");
        let next = text_window(1, &bytes[1..], size);
        assert_eq!((next.start, next.end), (1, 3));
        assert_eq!(next.text, "\u{e9}");
    }
}

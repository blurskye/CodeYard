//! The `.diff` and `.patch` media types, which `gh pr diff` and `git am` read.
use axum::http::header;
use axum::http::HeaderMap;
use axum::response::Response;
use codeyard_app::repositories::store::RepoStore;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git::CommitInfo;
use codeyard_git::DiffFileInfo;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Media {
    Diff,
    Patch,
}

/// The diff or patch media type in the Accept header, e.g. `application/vnd.github.v3.diff`.
pub(super) fn requested(headers: &HeaderMap) -> Option<Media> {
    let accept = headers.get(header::ACCEPT)?.to_str().ok()?;
    accept.split(',').find_map(|kind| {
        let kind = kind.split(';').next()?.trim();
        if !kind.starts_with("application/vnd.github") {
            None
        } else if kind.ends_with(".diff") {
            Some(Media::Diff)
        } else if kind.ends_with(".patch") {
            Some(Media::Patch)
        } else {
            None
        }
    })
}

/// Each file's patch already starts with its `diff --git` and file headers from Git.
fn diff(files: &[DiffFileInfo]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for file in files {
        let patch = if file.patch_bytes.is_empty() {
            file.patch.as_bytes()
        } else {
            &file.patch_bytes
        };
        bytes.extend_from_slice(patch);
        if !patch.is_empty() && !patch.ends_with(b"\n") {
            bytes.push(b'\n');
        }
    }
    bytes
}

fn patch(commit: &CommitInfo, files: &[DiffFileInfo]) -> Vec<u8> {
    let date = chrono::DateTime::from_timestamp(commit.timestamp, 0)
        .unwrap_or_default()
        .to_rfc2822();
    let (subject, body) = commit
        .message
        .trim_end()
        .split_once('\n')
        .map_or((commit.message.trim_end(), ""), |(subject, body)| {
            (subject, body.trim_start_matches('\n'))
        });
    let body = if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
    };
    let mut bytes = format!(
        "From {} Mon Sep 17 00:00:00 2001\nFrom: {} <{}>\nDate: {date}\nSubject: [PATCH] {subject}\n\n{body}---\n",
        commit.sha, commit.author_name, commit.author_email,
    ).into_bytes();
    bytes.extend_from_slice(&diff(files));
    bytes
}

/// The largest patch series answered in one response.
const MAX_SERIES_BYTES: usize = 16 * 1024 * 1024;

/// Each commit as its own patch, oldest first, as `git format-patch --stdout` writes a series.
/// Merge commits are left out, as format-patch leaves them out. `total` counts the range's
/// commits: a series missing some would not rebuild the head, so it is refused.
pub(super) async fn series(
    store: &RepoStore,
    commits: &[CommitInfo],
    total: usize,
) -> Result<Response, RepoError> {
    if commits.len() < total {
        return Err(RepoError::new(
            ErrorKind::Invalid,
            "The patch series is too large; fetch the commits instead",
        ));
    }
    let mut text = Vec::new();
    for commit in commits.iter().filter(|commit| commit.parents.len() <= 1) {
        let diff = store.commit_diff(&commit.sha).await?;
        text.extend_from_slice(&patch(commit, &diff.files));
        if text.len() > MAX_SERIES_BYTES {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "The patch series is too large; fetch the commits instead",
            ));
        }
    }
    Ok(([(header::CONTENT_TYPE, "text/plain")], text).into_response())
}

/// The files as a plain-text diff, or as a patch headed by `commit` for the patch type.
pub(super) fn response(media: Media, commit: &CommitInfo, files: &[DiffFileInfo]) -> Response {
    let text = match media {
        Media::Diff => diff(files),
        Media::Patch => patch(commit, files),
    };
    ([(header::CONTENT_TYPE, "text/plain")], text).into_response()
}

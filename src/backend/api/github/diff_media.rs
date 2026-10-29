//! The `.diff` and `.patch` media types, which `gh pr diff` and `git am` read.
use crate::backend::repositories::store::{
    CommitInfo, DiffFileInfo, ErrorKind, RepoError, RepoStore,
};
use axum::{
    http::{header, HeaderMap},
    response::{IntoResponse, Response},
};

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
fn diff(files: &[DiffFileInfo]) -> String {
    let mut text = String::new();
    for file in files {
        text.push_str(&file.patch);
        if !file.patch.is_empty() && !file.patch.ends_with('\n') {
            text.push('\n');
        }
    }
    text
}

fn patch(commit: &CommitInfo, files: &[DiffFileInfo]) -> String {
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
    format!(
        "From {} Mon Sep 17 00:00:00 2001\nFrom: {} <{}>\nDate: {date}\nSubject: [PATCH] {subject}\n\n{body}---\n{}",
        commit.sha,
        commit.author_name,
        commit.author_email,
        diff(files)
    )
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
    let mut text = String::new();
    for commit in commits.iter().filter(|commit| commit.parents.len() <= 1) {
        let diff = store.commit_diff(&commit.sha).await?;
        text.push_str(&patch(commit, &diff.files));
        if text.len() > MAX_SERIES_BYTES {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "The patch series is too large; fetch the commits instead",
            ));
        }
    }
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

/// The files as a plain-text diff, or as a patch headed by `commit` for the patch type.
pub(super) fn response(media: Media, commit: &CommitInfo, files: &[DiffFileInfo]) -> Response {
    let text = match media {
        Media::Diff => diff(files),
        Media::Patch => patch(commit, files),
    };
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
}

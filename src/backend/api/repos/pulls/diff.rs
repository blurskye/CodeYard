use crate::backend::api::context::PageContext;
use crate::backend::api::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::pulls::pull::*;
use crate::backend::api::repos::pulls::review_forms;
use crate::backend::api::repos::references::resolve_references_or_none;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::store::repo_store;
use crate::backend::repositories::store::{DiffFileInfo, DiffInfo};
use crate::webui::pages::collaboration as view;
use crate::webui::pages::collaboration::{Kind, LineComment};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
pub async fn get_pull_request_diff(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
) -> Result<Json<DiffInfo>, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    "database_error",
                    "Collaboration storage is temporarily unavailable",
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

    let pull_request =
        db::pull_requests::records::get_pull_request(&state.pool, repository.id, number)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError::new(
                        "database_error",
                        "Collaboration storage is temporarily unavailable",
                    )),
                )
            })?
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ApiError::new("not_found", "Pull request not found")),
                )
            })?;

    let diff = repo_store(&repository, state.cluster.as_ref())
        .diff(&pull_request.base_sha, &pull_request.head_sha)
        .await
        .map_err(|error| {
            if error.is_response_too_large() {
                crate::backend::api::repos::responses::repo_store_error(
                    error,
                    StatusCode::INTERNAL_SERVER_ERROR,
                )
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError::new(
                        "git_error",
                        "Collaboration storage is temporarily unavailable",
                    )),
                )
            }
        })?;

    Ok(Json(diff))
}

pub(crate) async fn native_diff(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    let composer = composer_from_query(&query);
    render_diff(&ctx, &repo, &value, composer.as_ref(), None, StatusCode::OK).await
}

/// The files a signed-in reader marked as viewed. A mark counts only while the file's blob id is
/// the one it was marked at, so a push that changes the file shows it unviewed again.
async fn viewed_marks(
    ctx: &PageContext,
    repo: &RepositoryData,
    pull_id: i64,
    number: i32,
    user_id: i64,
    files: &[DiffFileInfo],
) -> ResultPage<view::Viewed> {
    let marks: HashMap<String, String> = db::pull_requests::viewed::list_pull_request_viewed_files(
        &ctx.state.pool,
        pull_id,
        user_id,
    )
    .await
    .map_err(|_| unavailable(ctx))?
    .into_iter()
    .collect();
    let paths: BTreeSet<String> = files
        .iter()
        .filter(|file| marks.get(&file.filename) == Some(&file.blob_sha))
        .map(|file| file.filename.clone())
        .collect();
    Ok(view::Viewed {
        csrf: repo.view.csrf.clone(),
        action: format!("{}/{number}/viewed", repo.view.family_url(Kind::Pull)),
        paths,
    })
}

/// The composer a gutter link opens: `side` and `line` name the line, `path` the file.
fn composer_from_query(query: &HashMap<String, String>) -> Option<LineComment> {
    let side = query.get("side")?;
    let line = query.get("line")?;
    if !matches!(side.as_str(), "LEFT" | "RIGHT") || !line.parse::<i32>().is_ok_and(|n| n >= 1) {
        return None;
    }
    Some(LineComment {
        path: query.get("path")?.clone(),
        side: side.clone(),
        line: line.clone(),
        in_reply_to: None,
        body: String::new(),
        preview: false,
        commit: String::new(),
    })
}

/// The Files changed page: the diff with its inline conversations, and a composer open under
/// `composer`'s line. A refused comment or resolve comes back here with its error.
pub(crate) async fn render_diff(
    ctx: &PageContext,
    repo: &RepositoryData,
    value: &view::Item,
    composer: Option<&LineComment>,
    error: Option<&str>,
    status: StatusCode,
) -> ResultPage<Response> {
    let pull =
        db::pull_requests::records::get_pull_request(&ctx.state.pool, repo.record.id, value.number)
            .await
            .map_err(|_| unavailable(ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found"))?;
    let diff = repo_store(&repo.record, ctx.state.cluster.as_ref())
        .diff(&pull.base_sha, &pull.head_sha)
        .await
        .map_err(|error| {
            if error.is_response_too_large() {
                ctx.error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Repository response exceeds its limit.",
                )
            } else {
                unavailable(ctx)
            }
        })?;
    let inline =
        db::pull_requests::reviews::list_pull_request_inline_comments(&ctx.state.pool, pull.id)
            .await
            .map_err(|_| unavailable(ctx))?;
    let truncated = inline.truncated;
    let threads = review_forms::threads(ctx, repo, &pull, inline.comments);
    let mut texts: Vec<&str> = threads
        .iter()
        .flat_map(|thread| {
            std::iter::once(thread.root.body.as_str())
                .chain(thread.replies.iter().map(|reply| reply.body.as_str()))
        })
        .collect();
    texts.extend(composer.map(|draft| draft.body.as_str()));
    let references = resolve_references_or_none(
        ctx,
        &repo.record,
        &repo.view.owner,
        &repo.view.name,
        &texts,
        false,
    )
    .await;
    let viewed = match ctx.user.as_ref() {
        Some(user) => {
            Some(viewed_marks(ctx, repo, pull.id, value.number, user.id, &diff.files).await?)
        }
        None => None,
    };
    let diff = view::Diff {
        additions: diff.additions,
        deletions: diff.deletions,
        files: diff
            .files
            .into_iter()
            .map(|file| view::DiffFile {
                filename: file.filename,
                status: file.status,
                additions: file.additions,
                deletions: file.deletions,
                patch: file.patch,
            })
            .collect(),
        viewed,
    };
    Ok(with_status(
        ctx.page_model(
            "Pull request diff",
            page(view::Page::Diff {
                repository: repo.view.clone(),
                item: value.clone(),
                diff,
                threads,
                truncated,
                composer: composer.cloned(),
                error: error.map(str::to_owned),
                references: Box::new(references),
                split: ctx.query.get("view").is_some_and(|view| view == "split"),
            }),
        ),
        status,
    ))
}

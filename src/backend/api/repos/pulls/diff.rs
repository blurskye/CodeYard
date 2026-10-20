use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::issues::reactions;
use crate::backend::api::repos::pulls::pull::*;
use crate::backend::api::repos::pulls::review_forms;
use crate::backend::api::repos::refs::references::resolve_references_or_none;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::code_owners::load_rules;
use crate::backend::repositories::store::repo_store;
use crate::backend::repositories::store::{DiffFileInfo, DiffInfo};
use crate::webui::pages::repos::{
    issues::{self as view, Kind},
    pulls::{self, LineComment, ReviewDraft},
};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
/// Bound the actual envelope, including drafts, pending threads, references and JSON escaping.
pub(crate) fn bound_thread_projection(
    envelope: &mut crate::webui::app::PageEnvelope,
) -> Result<(), serde_json::Error> {
    use crate::webui::app::Page;
    let Page::Collaboration(model) = &mut envelope.page else {
        return Ok(());
    };
    let view::Page::Diff { threads, .. } = model.as_mut() else {
        return Ok(());
    };
    let entries = std::mem::take(threads);
    let mut bytes = serde_json::to_vec(envelope)?.len();
    let Page::Collaboration(model) = &mut envelope.page else {
        unreachable!()
    };
    let view::Page::Diff {
        threads, truncated, ..
    } = model.as_mut()
    else {
        unreachable!()
    };
    if bytes > crate::webui::bootstrap::MAX_PAGE_MODEL_BYTES {
        return Err(serde::ser::Error::custom(
            "Diff page exceeds its size limit",
        ));
    }
    for thread in entries {
        let size = serde_json::to_vec(&thread)?.len() + usize::from(!threads.is_empty());
        if bytes.saturating_add(size) > crate::webui::bootstrap::MAX_PAGE_MODEL_BYTES {
            *truncated = true;
            break;
        }
        bytes += size;
        threads.push(thread);
    }
    Ok(())
}

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
            crate::backend::api::repos::responses::repo_store_error(
                error,
                StatusCode::INTERNAL_SERVER_ERROR,
            )
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
    render_diff(
        &ctx,
        &repo,
        &value,
        composer.as_ref(),
        None,
        None,
        StatusCode::OK,
    )
    .await
}

/// The files a signed-in reader marked as viewed. A mark counts only while the file's blob id is
/// the one it was marked at, so a push that changes the file shows it unviewed again.
async fn viewed_marks(
    ctx: &PageContext,
    repo: &RepositoryData,
    pull_id: i64,
    pull: &db::models::PullRequest,
    user_id: i64,
    files: &[DiffFileInfo],
) -> ResultPage<pulls::Viewed> {
    let displayed: Vec<String> = files.iter().map(|file| file.filename.clone()).collect();
    let marks: HashMap<String, String> = db::pull_requests::viewed::list_pull_request_viewed_files(
        &ctx.state.pool,
        pull_id,
        user_id,
        &displayed,
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
    Ok(pulls::Viewed {
        csrf: repo.view.csrf.clone(),
        action: format!(
            "{}/{}/viewed",
            repo.view.family_url(Kind::Pull),
            pull.number
        ),
        head: pull.head_sha.clone(),
        base: pull.base_sha.clone(),
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
        repository_id: None,
        path: query.get("path")?.clone(),
        side: side.clone(),
        line: line.clone(),
        in_reply_to: None,
        body: String::new(),
        preview: false,
        commit: String::new(),
        base: String::new(),
    })
}

/// The diff as the Files changed page, a comparison and a commit page show it. `viewed` holds the
/// reader's Viewed marks where a pull request has them.
pub(crate) fn diff_model(diff: DiffInfo, viewed: Option<pulls::Viewed>) -> pulls::Diff {
    pulls::Diff {
        additions: diff.additions,
        deletions: diff.deletions,
        files: diff
            .files
            .into_iter()
            .map(|file| pulls::DiffFile {
                filename: file.filename,
                status: file.status,
                additions: file.additions,
                deletions: file.deletions,
                patch: file.patch,
                owners: Vec::new(),
            })
            .collect(),
        viewed,
    }
}

/// The Files changed page: the diff with its inline conversations, and a composer open under
/// `composer`'s line. A refused comment or resolve comes back here with its error, and a refused
/// review comes back with its text in the review form.
pub(crate) async fn render_diff(
    ctx: &PageContext,
    repo: &RepositoryData,
    value: &view::Item,
    composer: Option<&LineComment>,
    review: Option<ReviewDraft>,
    error: Option<&str>,
    status: StatusCode,
) -> ResultPage<Response> {
    let mut pull =
        db::pull_requests::records::get_pull_request(&ctx.state.pool, repo.record.id, value.number)
            .await
            .map_err(|_| unavailable(ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found"))?;
    // Keep the patch, thread state and form fields on the item's revision snapshot.
    if let Some(compare) = &value.comparison {
        pull.head_sha.clone_from(&compare.head_sha);
        pull.base_sha.clone_from(&compare.base_sha);
    }
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
    // The viewer's own pending comments show here, and nobody else's do.
    let pending = match ctx.user.as_ref() {
        Some(user) => db::pull_requests::reviews::list_pending_review_comments(
            &ctx.state.pool,
            pull.id,
            user.id,
        )
        .await
        .map_err(|_| unavailable(ctx))?,
        None => Vec::new(),
    };
    let pending_ids: HashSet<i64> = pending.iter().map(|entry| entry.comment.id).collect();
    let pending_comments = pending.len();
    let mut entries = inline.comments;
    entries.extend(pending);
    let mut threads = review_forms::threads(ctx, repo, &pull, entries, &pending_ids);
    reactions::attach_threads(ctx, &mut threads).await?;
    let mut texts: Vec<&str> = threads
        .iter()
        .flat_map(|thread| {
            std::iter::once(thread.root.body.as_str())
                .chain(thread.replies.iter().map(|reply| reply.body.as_str()))
        })
        .collect();
    texts.extend(composer.map(|draft| draft.body.as_str()));
    texts.extend(review.as_ref().map(|draft| draft.body.as_str()));
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
        Some(user) => Some(viewed_marks(ctx, repo, pull.id, &pull, user.id, &diff.files).await?),
        None => None,
    };
    let mut diff = diff_model(diff, viewed);
    // A CODEOWNERS file that cannot be read leaves the owner chips out; the page still shows.
    let rules = load_rules(&repo.record, ctx.state.cluster.as_ref(), &pull.base_sha)
        .await
        .unwrap_or_default();
    let paths: Vec<String> = diff
        .files
        .iter()
        .map(|file| file.filename.clone())
        .collect();
    let owned: HashMap<String, &[String]> = rules
        .owned_paths(&paths)
        .unwrap_or_default()
        .into_iter()
        .collect();
    for file in &mut diff.files {
        file.owners = owned
            .get(&file.filename)
            .copied()
            .unwrap_or_default()
            .to_vec();
    }
    let mut ctx = ctx.clone();
    let split = ctx.query.get("view").is_some_and(|view| view == "split");
    ctx.return_to = Some(format!(
        "{}/{}/diff{}",
        repo.view.family_url(Kind::Pull),
        value.number,
        if split { "?view=split" } else { "" }
    ));
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
                review,
                pending_comments,
                error: error.map(str::to_owned),
                references: Box::new(references),
                split,
            }),
        ),
        status,
    ))
}

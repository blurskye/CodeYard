//! Native review forms for pull requests: reviews and their dismissal, review requests, inline
//! comments written from the diff, conversation resolution and Viewed marks on files. Each form checks the head it was
//! written against, then calls the repository function its REST endpoint calls, so the rules
//! stay in one place. The review data the Conversation tab and the diff show is built here too.
use crate::backend::api::context::PageContext;
use crate::backend::api::repos::pulls::diff::render_diff;
use crate::backend::api::repos::pulls::operation_error;
use crate::backend::api::repos::pulls::pull::*;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::pull_requests::reviews::{PullRequestCommentEntry, PullRequestReviewEntry};
use crate::backend::issues::comment_link;
use crate::backend::repositories::pull_requests::comments as comment_rules;
use crate::backend::repositories::pull_requests::comments::CreatePullRequestCommentRequest;
use crate::backend::repositories::pull_requests::reviews as review_rules;
use crate::backend::repositories::pull_requests::reviews::{
    CreatePullRequestReviewRequest, DismissPullRequestReviewRequest, RequestReviewerRequest,
};
use crate::backend::repositories::store::repo_store;
use crate::webui::pages::collaboration::{
    self as view, DetailDraft, Kind, LineComment, ReviewDraft,
};
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;

/// Reviews are loaded for one discussion page's time span. A pull request with more reviews than
/// this shows the oldest ones only.
const TIMELINE_REVIEW_LIMIT: i64 = 1_000;

/// The Conversation tab's state for a root comment on a file. Resolving is offered to the pull
/// request author and to anyone who may write; the resolve handler checks both again.
pub(crate) fn conversation(
    ctx: &PageContext,
    repo: &RepositoryData,
    pull_author_id: i64,
    head: &str,
    comment: &db::models::PullRequestComment,
) -> view::Conversation {
    let viewer = ctx.user.as_ref();
    view::Conversation {
        resolved: comment.resolved_at.is_some(),
        outdated: comment.commit_sha.as_deref() != Some(head),
        can_resolve: viewer.is_some_and(|user| user.id == pull_author_id || repo.view.can_write),
        commit: comment.commit_sha.clone().unwrap_or_default(),
    }
}

/// One discussion comment: its author, its text and, for a file comment, its location.
pub(crate) fn comment_view(
    entry: PullRequestCommentEntry,
    conversation: Option<view::Conversation>,
) -> view::Comment {
    let author = entry
        .author
        .map(|author| author.username)
        .unwrap_or_else(|| "Deleted user".into());
    let comment = entry.comment;
    let location = comment.path.map(|path| match comment.line {
        Some(line) => format!(
            "{path}:{line} ({})",
            comment.side.as_deref().unwrap_or("diff")
        ),
        None => path,
    });
    view::Comment {
        id: comment.id,
        author,
        body: comment.body,
        created: comment.created_at.to_rfc3339(),
        location,
        conversation,
    }
}

/// The inline conversations of a pull request. Replies join the root of their chain. A reply
/// whose chain leaves the loaded list has no place to show, so it is left out.
pub(crate) fn threads(
    ctx: &PageContext,
    repo: &RepositoryData,
    pull: &db::models::PullRequest,
    entries: Vec<PullRequestCommentEntry>,
) -> Vec<view::Thread> {
    let parents: HashMap<i64, Option<i64>> = entries
        .iter()
        .map(|entry| (entry.comment.id, entry.comment.in_reply_to_comment_id))
        .collect();
    let root_of = |id: i64| -> Option<i64> {
        let mut current = id;
        // A chain longer than the list must repeat a comment, so it has no root.
        for _ in 0..=parents.len() {
            match parents.get(&current)? {
                None => return Some(current),
                Some(parent) => current = *parent,
            }
        }
        None
    };
    let mut replies: HashMap<i64, Vec<view::Comment>> = HashMap::new();
    let mut roots = Vec::new();
    for entry in entries {
        if entry.comment.in_reply_to_comment_id.is_none() {
            roots.push(entry);
        } else if let Some(root) = root_of(entry.comment.id) {
            replies
                .entry(root)
                .or_default()
                .push(comment_view(entry, None));
        }
    }
    roots
        .into_iter()
        .map(|entry| {
            let id = entry.comment.id;
            let state = conversation(ctx, repo, pull.author_id, &pull.head_sha, &entry.comment);
            view::Thread {
                path: entry.comment.path.clone().unwrap_or_default(),
                line: entry.comment.line,
                side: entry.comment.side.clone().unwrap_or_default(),
                replies: replies.remove(&id).unwrap_or_default(),
                root: comment_view(entry, Some(state)),
            }
        })
        .collect()
}

fn review_view(entry: PullRequestReviewEntry, head: &str) -> view::Review {
    let author = entry
        .author
        .map(|author| author.username)
        .unwrap_or_else(|| "Deleted user".into());
    let review = entry.review;
    view::Review {
        id: review.id,
        author,
        state: review.state,
        body: review.body,
        commit: review.commit_sha.clone().unwrap_or_default(),
        created: review.created_at.to_rfc3339(),
        stale: review.commit_sha.as_deref() != Some(head),
        dismissed_reason: review.dismissed_reason,
    }
}

/// The reviews created at or after `after` and before `before`, oldest first. A discussion page
/// passes its first comment's time and the next page's, so every review shows on one page. The
/// second value is the load limit when the pull request has more reviews than that.
pub(crate) async fn timeline_reviews(
    ctx: &PageContext,
    pull: &db::models::PullRequest,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
) -> ResultPage<(Vec<view::Review>, Option<usize>)> {
    let entries = db::pull_requests::reviews::list_pull_request_reviews(
        &ctx.state.pool,
        pull.id,
        TIMELINE_REVIEW_LIMIT,
        0,
    )
    .await
    .map_err(|_| unavailable(ctx))?;
    let limit =
        (entries.len() as i64 >= TIMELINE_REVIEW_LIMIT).then_some(TIMELINE_REVIEW_LIMIT as usize);
    let reviews = entries
        .into_iter()
        .filter(|entry| {
            after.is_none_or(|after| entry.review.created_at >= after)
                && before.is_none_or(|before| entry.review.created_at < before)
        })
        .map(|entry| review_view(entry, &pull.head_sha))
        .collect();
    Ok((reviews, limit))
}

/// The sidebar reviewers: each one's latest decision, then the review requests still open.
pub(crate) async fn reviewers(
    ctx: &PageContext,
    repo: &RepositoryData,
    number: i32,
    head: &str,
) -> ResultPage<Vec<view::Reviewer>> {
    let pull =
        db::pull_requests::records::get_pull_request(&ctx.state.pool, repo.record.id, number)
            .await
            .map_err(|_| unavailable(ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found"))?;
    let decisions =
        db::pull_requests::reviews::list_pull_request_review_decisions(&ctx.state.pool, pull.id)
            .await
            .map_err(|_| unavailable(ctx))?;
    let requested =
        db::pull_requests::reviews::list_pull_request_reviewers(&ctx.state.pool, pull.id)
            .await
            .map_err(|_| unavailable(ctx))?;
    let mut reviewers: Vec<view::Reviewer> = decisions
        .into_iter()
        .filter_map(|decision| {
            let status = match decision.state.as_str() {
                "approved" => view::ReviewerStatus::Approved,
                "changes_requested" => view::ReviewerStatus::ChangesRequested,
                "dismissed" => view::ReviewerStatus::Dismissed,
                _ => return None,
            };
            Some(view::Reviewer {
                username: decision.username.unwrap_or_else(|| "Deleted user".into()),
                status,
                stale: status != view::ReviewerStatus::Dismissed
                    && decision.commit_sha.as_deref() != Some(head),
                pending: requested.iter().any(|user| user.id == decision.author_id),
            })
        })
        .collect();
    for user in requested {
        if !reviewers
            .iter()
            .any(|reviewer| reviewer.username == user.username)
        {
            reviewers.push(view::Reviewer {
                username: user.username,
                status: view::ReviewerStatus::Requested,
                stale: false,
                pending: true,
            });
        }
    }
    Ok(reviewers)
}

/// Shows a refused review again with the reviewer's text kept.
async fn refuse_review(
    ctx: &PageContext,
    repo: &RepositoryData,
    value: &view::Item,
    review: ReviewDraft,
    error: Option<&str>,
    status: StatusCode,
) -> ResultPage<Response> {
    let draft = DetailDraft {
        review: Some(review),
        ..DetailDraft::default()
    };
    render_detail(ctx, repo, Kind::Pull, value, &draft, error, status).await
}

/// Shows a refused comment again. A comment from the diff reopens its composer on the diff; one
/// from the Conversation tab reopens there.
async fn refuse_comment(
    ctx: &PageContext,
    repo: &RepositoryData,
    value: &view::Item,
    from_diff: bool,
    line: &LineComment,
    error: Option<&str>,
    status: StatusCode,
) -> ResultPage<Response> {
    if from_diff {
        render_diff(ctx, repo, value, Some(line), error, status).await
    } else {
        let draft = DetailDraft {
            comment: line.body.clone(),
            preview: line.preview,
            ..DetailDraft::default()
        };
        render_detail(ctx, repo, Kind::Pull, value, &draft, error, status).await
    }
}

pub(crate) async fn native_review(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    // The commit the reviewer saw travels with the draft, so a later submit is still checked
    // against it, even when the head has moved in the meantime.
    let submitted = ReviewDraft {
        state: field(&form, "state"),
        body: field(&form, "body"),
        preview: field(&form, "intent") == "preview",
        commit: field(&form, "commit_sha"),
    };
    if submitted.preview {
        return refuse_review(&ctx, &repo, &value, submitted, None, StatusCode::OK).await;
    }
    let compare = value
        .comparison
        .as_ref()
        .ok_or_else(|| ctx.error(StatusCode::CONFLICT, "Comparison data is unavailable."))?;
    // Early feedback only. The repository function checks the stored head in its own transaction.
    if field(&form, "commit_sha") != compare.head_sha {
        return refuse_review(
            &ctx,
            &repo,
            &value,
            submitted,
            Some("The pull request changed after this review was started. Reload the page before submitting."),
            StatusCode::CONFLICT,
        )
        .await;
    }
    let request = CreatePullRequestReviewRequest {
        state: Some(submitted.state.clone()).filter(|state| !state.is_empty()),
        body: (!submitted.body.trim().is_empty()).then(|| submitted.body.clone()),
        commit_sha: Some(compare.head_sha.clone()),
    };
    let user = ctx.require_user()?.id;
    match review_rules::create_pull_request_review(
        &ctx.state.pool,
        ctx.state.cluster.as_ref(),
        &ctx.state.notification_broadcaster,
        user,
        (owner.clone(), name.clone(), number),
        request,
    )
    .await
    .map_err(operation_error)
    {
        Ok(review) => Ok(ctx.redirect(&format!(
            "{}/{}#review-{}",
            repo.view.family_url(Kind::Pull),
            number,
            review.id
        ))),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            refuse_review(&ctx, &repo, &value, submitted, Some(&message), status).await
        }
    }
}

pub(crate) async fn native_dismiss_review(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number, review_id)): Path<(String, String, i32, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    let user = ctx.require_user()?.id;
    let reason = field(&form, "reason");
    let request = DismissPullRequestReviewRequest {
        reason: (!reason.trim().is_empty()).then_some(reason),
    };
    match review_rules::dismiss_pull_request_review(
        &ctx.state.pool,
        user,
        owner,
        name,
        number,
        review_id,
        request,
    )
    .await
    .map_err(operation_error)
    {
        Ok(_) => Ok(ctx.redirect(&format!(
            "{}/{}#review-{}",
            repo.view.family_url(Kind::Pull),
            number,
            review_id
        ))),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            render_detail(
                &ctx,
                &repo,
                Kind::Pull,
                &value,
                &DetailDraft::default(),
                Some(&message),
                status,
            )
            .await
        }
    }
}

/// Asks a user for a review through the rules of the REST route. A refused request keeps the
/// name that was typed.
pub(crate) async fn native_request_reviewer(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    let user = ctx.require_user()?.id;
    let base = format!("{}/{}", repo.view.family_url(Kind::Pull), number);
    let username = field(&form, "username");
    let request = RequestReviewerRequest {
        username: username.clone(),
    };
    match review_rules::request_reviewer(
        &ctx.state.pool,
        &ctx.state.notification_broadcaster,
        user,
        owner,
        name,
        number,
        request,
    )
    .await
    .map_err(operation_error)
    {
        Ok(_) => Ok(ctx.redirect(&format!("{base}#reviewers"))),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            let draft = DetailDraft {
                reviewer: username,
                ..DetailDraft::default()
            };
            render_detail(
                &ctx,
                &repo,
                Kind::Pull,
                &value,
                &draft,
                Some(&message),
                status,
            )
            .await
        }
    }
}

/// Takes back an open review request, through the rules of the REST route.
pub(crate) async fn native_remove_requested_reviewer(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number, reviewer)): Path<(String, String, i32, String)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    let user = ctx.require_user()?.id;
    let base = format!("{}/{}", repo.view.family_url(Kind::Pull), number);
    match review_rules::remove_requested_reviewer(
        &ctx.state.pool,
        user,
        owner,
        name,
        number,
        reviewer,
    )
    .await
    .map_err(operation_error)
    {
        Ok(()) => Ok(ctx.redirect(&format!("{base}#reviewers"))),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            render_detail(
                &ctx,
                &repo,
                Kind::Pull,
                &value,
                &DetailDraft::default(),
                Some(&message),
                status,
            )
            .await
        }
    }
}

/// A comment on the Conversation tab, or on a line or thread of the diff. The diff sends the
/// head it showed as `commit_sha`, which must still be the pull request head.
pub(crate) async fn native_pull_comment(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    let line = LineComment {
        path: field(&form, "path"),
        side: field(&form, "side"),
        line: field(&form, "line"),
        in_reply_to: form.get("in_reply_to").and_then(|id| id.parse().ok()),
        body: field(&form, "body"),
        preview: field(&form, "intent") == "preview",
        commit: field(&form, "commit_sha"),
    };
    let from_diff = field(&form, "from") == "diff";
    if line.preview {
        return refuse_comment(&ctx, &repo, &value, from_diff, &line, None, StatusCode::OK).await;
    }
    let compare = value
        .comparison
        .as_ref()
        .ok_or_else(|| ctx.error(StatusCode::CONFLICT, "Comparison data is unavailable."))?;
    let head = compare.head_sha.as_str();
    let inline = !line.path.is_empty();
    if let Err(message) = crate::backend::issues::validate_body(&line.body, true) {
        return refuse_comment(
            &ctx,
            &repo,
            &value,
            from_diff,
            &line,
            Some(message),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    if inline && field(&form, "commit_sha") != head {
        return refuse_comment(
            &ctx,
            &repo,
            &value,
            from_diff,
            &line,
            Some("The pull request changed after this comment was started. Reload the diff before commenting."),
            StatusCode::CONFLICT,
        )
        .await;
    }
    let number_line = match line.line.as_str() {
        "" => None,
        text => match text.parse::<i32>() {
            Ok(number) => Some(number),
            Err(_) => {
                return refuse_comment(
                    &ctx,
                    &repo,
                    &value,
                    from_diff,
                    &line,
                    Some("Choose a line in the diff to comment on."),
                    StatusCode::UNPROCESSABLE_ENTITY,
                )
                .await
            }
        },
    };
    let request = CreatePullRequestCommentRequest {
        body: line.body.clone(),
        path: inline.then(|| line.path.clone()),
        line: number_line,
        side: (!line.side.is_empty()).then(|| line.side.clone()),
        commit_sha: inline.then(|| head.to_owned()),
        in_reply_to_comment_id: line.in_reply_to,
    };
    let user = ctx.require_user()?.id;
    match comment_rules::create_pull_request_comment(
        &ctx.state.pool,
        ctx.state.cluster.as_ref(),
        &ctx.state.notification_broadcaster,
        user,
        owner,
        name,
        number,
        request,
    )
    .await
    .map_err(operation_error)
    {
        Ok(comment) => {
            let base = format!("{}/{}", repo.view.family_url(Kind::Pull), number);
            if from_diff {
                let root = comment.in_reply_to_comment_id.unwrap_or(comment.id);
                // The composer posts to a URL that keeps `view=split` when the comment came from there.
                let view = if ctx.query.get("view").is_some_and(|view| view == "split") {
                    "?view=split"
                } else {
                    ""
                };
                Ok(ctx.redirect(&format!("{base}/diff{view}#thread-{root}")))
            } else {
                let position = db::pull_requests::reviews::count_pull_request_comments(
                    &ctx.state.pool,
                    comment.pull_request_id,
                )
                .await;
                Ok(ctx.redirect(&comment_link(&base, comment.id, position)))
            }
        }
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            refuse_comment(
                &ctx,
                &repo,
                &value,
                from_diff,
                &line,
                Some(&message),
                status,
            )
            .await
        }
    }
}

pub(crate) async fn native_resolve_comment(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number, comment_id)): Path<(String, String, i32, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    set_resolution(
        state,
        headers,
        uri,
        form,
        (owner, name, number, comment_id),
        true,
    )
    .await
}

pub(crate) async fn native_unresolve_comment(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number, comment_id)): Path<(String, String, i32, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    set_resolution(
        state,
        headers,
        uri,
        form,
        (owner, name, number, comment_id),
        false,
    )
    .await
}

/// Marks one file of a pull request as viewed, or clears the mark. The mark is stored against the
/// file's blob id in the current diff, so the browser cannot choose it.
pub(crate) async fn native_set_viewed(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let user = ctx.require_user()?.id;
    let pull =
        db::pull_requests::records::get_pull_request(&ctx.state.pool, repo.record.id, number)
            .await
            .map_err(|_| unavailable(&ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found"))?;
    let diff = repo_store(&repo.record, ctx.state.cluster.as_ref())
        .diff(&pull.base_sha, &pull.head_sha)
        .await
        .map_err(|_| unavailable(&ctx))?;
    let base = format!("{}/{}", repo.view.family_url(Kind::Pull), number);
    let path = field(&form, "path");
    let Some((index, file)) = diff
        .files
        .iter()
        .enumerate()
        .find(|(_, file)| file.filename == path)
    else {
        // The file left the diff, for example after a push. Nothing is marked.
        return Ok(ctx.redirect(&format!("{base}/diff")));
    };
    let blob = (field(&form, "viewed") == "yes").then_some(file.blob_sha.as_str());
    db::pull_requests::viewed::set_pull_request_file_viewed(
        &ctx.state.pool,
        pull.id,
        user,
        &path,
        blob,
    )
    .await
    .map_err(|_| unavailable(&ctx))?;
    Ok(ctx.redirect(&format!("{base}/diff#diff-file-{index}")))
}

/// Resolves or reopens a conversation, then returns to its thread on the page it was sent from.
async fn set_resolution(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    form: HashMap<String, String>,
    (owner, name, number, comment_id): (String, String, i32, i64),
    resolved: bool,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    let user = ctx.require_user()?.id;
    let from_diff = field(&form, "from") == "diff";
    let base = format!("{}/{}", repo.view.family_url(Kind::Pull), number);
    match comment_rules::set_comment_resolved(
        &ctx.state.pool,
        user,
        &owner,
        &name,
        number,
        comment_id,
        resolved,
    )
    .await
    .map_err(operation_error)
    {
        Ok(_) => {
            let location = if from_diff {
                format!("{base}/diff#thread-{comment_id}")
            } else {
                match field(&form, "page").parse::<usize>() {
                    Ok(page) if page > 1 => format!("{base}?page={page}#thread-{comment_id}"),
                    _ => format!("{base}#thread-{comment_id}"),
                }
            };
            Ok(ctx.redirect(&location))
        }
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            if from_diff {
                render_diff(&ctx, &repo, &value, None, Some(&message), status).await
            } else {
                render_detail(
                    &ctx,
                    &repo,
                    Kind::Pull,
                    &value,
                    &DetailDraft::default(),
                    Some(&message),
                    status,
                )
                .await
            }
        }
    }
}

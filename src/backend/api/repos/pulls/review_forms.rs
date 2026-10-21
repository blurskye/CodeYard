//! Native review forms for pull requests: reviews and their dismissal, review requests, inline
//! comments written from the diff, conversation resolution and Viewed marks on files. Each form checks the head it was
//! written against, then calls the repository function its REST endpoint calls, so the rules
//! stay in one place. The review data the Conversation tab and the diff show is built here too.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::pulls::diff::render_diff;
use crate::backend::api::repos::pulls::operation_error;
use crate::backend::api::repos::pulls::pull::*;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::pull_requests::reviews::{PullRequestCommentEntry, PullRequestReviewEntry};
use crate::backend::issues::{
    discussion_link, discussion_link_at, discussion_page, MAX_REVIEW_COMMENT_OFFSET,
};
use crate::backend::repositories::code_owners::{load_rules, resolve_owners};
use crate::backend::repositories::pull_requests::comments as comment_rules;
use crate::backend::repositories::pull_requests::comments::CreatePullRequestCommentRequest;
use crate::backend::repositories::pull_requests::reviews as review_rules;
use crate::backend::repositories::pull_requests::reviews::{
    CreatePullRequestReviewRequest, DismissPullRequestReviewRequest, RequestReviewerRequest,
};
use crate::backend::repositories::store::repo_store;
use crate::webui::pages::repos::{
    issues::{self as view, DetailDraft, Kind},
    pulls::{self, LineComment, ReviewDraft},
};
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// The Conversation tab's state for a root comment on a file. Resolving is offered to the pull
/// request author and to anyone who may write; the resolve handler checks both again.
pub(crate) fn conversation(
    ctx: &PageContext,
    repo: &RepositoryData,
    pull_author_id: i64,
    head: &str,
    comment: &db::models::PullRequestComment,
) -> pulls::Conversation {
    let viewer = ctx.user.as_ref();
    pulls::Conversation {
        resolved: comment.resolved_at.is_some(),
        outdated: comment.commit_sha.as_deref() != Some(head),
        can_resolve: viewer.is_some_and(|user| user.id == pull_author_id || repo.view.can_write),
        commit: comment.commit_sha.clone().unwrap_or_default(),
    }
}

/// One discussion comment: its author, its text and, for a file comment, its location.
pub(crate) fn comment_view(
    entry: PullRequestCommentEntry,
    conversation: Option<pulls::Conversation>,
) -> view::Comment {
    let author = entry.author.username;
    let comment = entry.comment;
    // The side is not shown. The thread's link on the Files changed tab marks the old and new lines.
    let location = comment.path.map(|path| match comment.line {
        Some(line) => format!("{path}:{line}"),
        None => path,
    });
    view::Comment {
        id: comment.id,
        author,
        revision: crate::backend::api::repos::issues::tasks::comment_revision(&comment.body),
        body: comment.body,
        created: comment.created_at.to_rfc3339(),
        location,
        conversation,
        reactions: Vec::new(),
        can_edit: false,
        can_delete: false,
        edited: None,
        edit: None,
    }
}

/// One discussion comment as the page shows it. A root comment on a file is a review conversation.
pub(crate) fn discussion_comment(
    ctx: &PageContext,
    repo: &RepositoryData,
    pull: &db::models::PullRequest,
    entry: PullRequestCommentEntry,
) -> view::Comment {
    let conversation = (entry.comment.path.is_some()
        && entry.comment.in_reply_to_comment_id.is_none())
    .then(|| conversation(ctx, repo, pull.author_id, &pull.head_sha, &entry.comment));
    comment_view(entry, conversation)
}

/// The inline conversations of a pull request. Replies join the root of their chain. A reply
/// whose chain leaves the loaded list has no place to show, so it is left out. `pending` holds
/// the comments of the viewer's own pending review.
pub(crate) fn threads(
    ctx: &PageContext,
    repo: &RepositoryData,
    pull: &db::models::PullRequest,
    entries: Vec<PullRequestCommentEntry>,
    pending: &HashSet<i64>,
) -> Vec<pulls::Thread> {
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
            pulls::Thread {
                path: entry.comment.path.clone().unwrap_or_default(),
                line: entry.comment.line,
                side: entry.comment.side.clone().unwrap_or_default(),
                pending: pending.contains(&id),
                replies: replies.remove(&id).unwrap_or_default(),
                root: comment_view(entry, Some(state)),
            }
        })
        .collect()
}

fn review_view(entry: PullRequestReviewEntry, head: &str) -> pulls::Review {
    let author = entry.author.username;
    let review = entry.review;
    pulls::Review {
        id: review.id,
        author,
        state: review.state,
        body: review.body,
        commit: review.commit_sha.clone().unwrap_or_default(),
        created: review.created_at.to_rfc3339(),
        stale: review.commit_sha.as_deref() != Some(head),
        dismissed_reason: review.dismissed_reason,
        comments: Vec::new(),
        comments_more: None,
        comments_start: None,
        reactions: Vec::new(),
        comments_offset: 0,
        can_dismiss: false,
    }
}

/// The reviews of one discussion page, oldest first. Each carries the inline comments filed in it.
pub(crate) async fn page_reviews(
    ctx: &PageContext,
    repo: &RepositoryData,
    pull: &db::models::PullRequest,
    entries: Vec<PullRequestReviewEntry>,
) -> ResultPage<Vec<pulls::Review>> {
    let ids: Vec<i64> = entries.iter().map(|entry| entry.review.id).collect();
    let offset = crate::backend::api::repos::chrome::number(
        ctx,
        "review_comments",
        0,
        MAX_REVIEW_COMMENT_OFFSET,
    )?;
    let comments =
        db::pull_requests::reviews::list_review_comments_page(&ctx.state.pool, &ids, offset as i64)
            .await
            .map_err(|_| unavailable(ctx))?;
    let next = offset + comments.comments.len();
    let has_more = comments.has_more;
    let mut reviews: Vec<pulls::Review> = entries
        .into_iter()
        .map(|entry| review_view(entry, &pull.head_sha))
        .collect();
    for entry in comments.comments {
        let Some(review_id) = entry.comment.review_id else {
            continue;
        };
        let comment = discussion_comment(ctx, repo, pull, entry);
        if let Some(review) = reviews.iter_mut().find(|review| review.id == review_id) {
            review.comments.push(comment);
        }
    }
    let page = crate::backend::api::repos::chrome::number(
        ctx,
        "page",
        1,
        crate::backend::issues::MAX_COMMENT_PAGE,
    )?;
    let base = format!(
        "{}/{}?page={page}",
        repo.view.family_url(Kind::Pull),
        pull.number
    );
    // Dismissing needs repository admin, and on a personal repository the owner or a site admin.
    let can_dismiss = repo.view.can_admin
        && ctx.user.as_ref().is_some_and(|user| {
            repo.record.owner_kind != "user"
                || user.is_admin
                || repo.record.is_personal_owner(user.id)
        });
    for review in &mut reviews {
        review.comments_offset = offset;
        review.can_dismiss = can_dismiss;
    }
    if let Some(last) = reviews.last_mut() {
        last.comments_more =
            has_more.then(|| format!("{base}&review_comments={next}#review-{}", last.id));
        last.comments_start = (offset > 0).then(|| format!("{base}#review-{}", last.id));
    }
    Ok(reviews)
}

/// The sidebar reviewers: each one's latest decision, then the review requests still open.
pub(crate) async fn reviewers(
    ctx: &PageContext,
    repo: &RepositoryData,
    number: i32,
    head: &str,
) -> ResultPage<Vec<pulls::Reviewer>> {
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
    // The owners the changed files name. A reviewer named there gets the Code owner badge.
    let rules = load_rules(&repo.record, ctx.state.cluster.as_ref(), &pull.base_sha)
        .await
        .unwrap_or_default();
    let changed = if rules.is_empty() {
        Vec::new()
    } else {
        repo_store(&repo.record, ctx.state.cluster.as_ref())
            .changed_paths(&pull.base_sha, &pull.head_sha)
            .await
            .unwrap_or_default()
    };
    let owned = rules.owned_paths(&changed).unwrap_or_default();
    let names: Vec<&str> = owned
        .iter()
        .flat_map(|(_, names)| names.iter().map(String::as_str))
        .collect();
    let owner_ids: HashSet<i64> = if names.is_empty() {
        HashSet::new()
    } else {
        resolve_owners(&ctx.state.pool, &repo.record, names)
            .await
            .unwrap_or_default()
            .into_values()
            .flatten()
            .collect()
    };
    let mut reviewers: Vec<pulls::Reviewer> = decisions
        .into_iter()
        .filter_map(|decision| {
            let status = match decision.state.as_str() {
                "approved" => pulls::ReviewerStatus::Approved,
                "changes_requested" => pulls::ReviewerStatus::ChangesRequested,
                "dismissed" => pulls::ReviewerStatus::Dismissed,
                _ => return None,
            };
            let code_owner = owner_ids.contains(&decision.author_id);
            Some(pulls::Reviewer {
                username: decision.username,
                status,
                stale: status != pulls::ReviewerStatus::Dismissed
                    && decision.commit_sha.as_deref() != Some(head),
                pending: requested.iter().any(|user| user.id == decision.author_id),
                code_owner,
            })
        })
        .collect();
    for user in requested {
        if !reviewers
            .iter()
            .any(|reviewer| reviewer.username == user.username)
        {
            let code_owner = owner_ids.contains(&user.id);
            reviewers.push(pulls::Reviewer {
                username: user.username,
                status: pulls::ReviewerStatus::Requested,
                stale: false,
                pending: true,
                code_owner,
            });
        }
    }
    Ok(reviewers)
}

/// Where a review is shown: its place in the Conversation, found from its own time.
async fn review_location(
    ctx: &PageContext,
    repo: &RepositoryData,
    number: i32,
    review: &db::models::PullRequestReview,
) -> String {
    let position = db::pull_requests::reviews::count_discussion_items_through(
        &ctx.state.pool,
        review.pull_request_id,
        db::pull_requests::reviews::DiscussionRef::Review(review.id),
    )
    .await;
    let base = format!("{}/{}", repo.view.family_url(Kind::Pull), number);
    discussion_link(
        &base,
        discussion_page(position),
        &format!("review-{}", review.id),
    )
}

/// Shows a refused review again with the reviewer's text kept. The review form is on the Files
/// changed page.
async fn refuse_review(
    ctx: &PageContext,
    repo: &RepositoryData,
    value: &view::Item,
    review: ReviewDraft,
    error: Option<&str>,
    status: StatusCode,
) -> ResultPage<Response> {
    render_diff(ctx, repo, value, None, Some(review), error, status).await
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
        render_diff(ctx, repo, value, Some(line), None, error, status).await
    } else {
        let draft = DetailDraft {
            comment: line.body.clone(),
            repository_id: line.repository_id.map(|id| id.to_string()),
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
        Ok(review) => {
            let location = review_location(&ctx, &repo, number, &review).await;
            Ok(ctx.redirect(&location))
        }
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
        Ok(review) => {
            let location = review_location(&ctx, &repo, number, &review).await;
            Ok(ctx.redirect(&location))
        }
        Err(failure) => {
            let value = item(&ctx, &repo, Kind::Pull, number).await?;
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
            let value = item(&ctx, &repo, Kind::Pull, number).await?;
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
            let value = item(&ctx, &repo, Kind::Pull, number).await?;
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
        repository_id: field(&form, "repository_id").parse().ok(),
        path: field(&form, "path"),
        side: field(&form, "side"),
        line: field(&form, "line"),
        in_reply_to: form.get("in_reply_to").and_then(|id| id.parse().ok()),
        body: field(&form, "body"),
        preview: field(&form, "intent") == "preview",
        commit: field(&form, "commit_sha"),
        base: field(&form, "base_sha"),
    };
    let from_diff = field(&form, "from") == "diff";
    let repository_id = line.repository_id;
    if repository_id != Some(repo.record.id) {
        return refuse_comment(
            &ctx,
            &repo,
            &value,
            from_diff,
            &line,
            Some(
                "The repository changed after this comment was started. Reload before commenting.",
            ),
            StatusCode::CONFLICT,
        )
        .await;
    }
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
    if inline && (line.commit != head || line.base != compare.base_sha) {
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
        repository_id,
        path: inline.then(|| line.path.clone()),
        line: number_line,
        side: (!line.side.is_empty()).then(|| line.side.clone()),
        commit_sha: inline.then(|| head.to_owned()),
        base_sha: inline.then(|| line.base.clone()),
        in_reply_to_comment_id: line.in_reply_to,
        // "Start a review" and "Add to review" file the comment in the pending review.
        pending: field(&form, "intent") == "review",
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
                let link =
                    crate::backend::repositories::pull_requests::comments::saved_comment_link(
                        &ctx.state.pool,
                        &base,
                        &comment,
                    )
                    .await;
                Ok(ctx.redirect(&link))
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
    if field(&form, "expected_head") != pull.head_sha
        || field(&form, "expected_base") != pull.base_sha
    {
        return Err(ctx.error(
            StatusCode::CONFLICT,
            "The pull request changed. Reload before marking this file.",
        ));
    }
    let base = format!("{}/{}", repo.view.family_url(Kind::Pull), number);
    let path = field(&form, "path");
    if path.len() > db::pull_requests::reviews::MAX_COMMENT_PATH_BYTES || path.contains('\0') {
        return Err(ctx.error(StatusCode::BAD_REQUEST, "Invalid diff path."));
    }
    let diff = repo_store(&repo.record, ctx.state.cluster.as_ref())
        .diff_file(&pull.base_sha, &pull.head_sha, &path)
        .await
        .map_err(|error| crate::backend::api::repos::chrome::store_error(&ctx, None, error))?;
    let blob_sha = diff
        .files
        .into_iter()
        .find(|file| file.filename == path)
        .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Choose a file in this diff."))?
        .blob_sha;
    let blob = (field(&form, "viewed") == "yes").then_some(blob_sha.as_str());
    db::pull_requests::viewed::set_pull_request_file_viewed(
        &ctx.state.pool,
        pull.id,
        user,
        &path,
        blob,
    )
    .await
    .map_err(|_| unavailable(&ctx))?;
    if field(&form, "background") == "yes"
        || ctx
            .headers
            .get(axum::http::header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            == Some("application/json")
    {
        return Ok(axum::response::IntoResponse::into_response(
            StatusCode::NO_CONTENT,
        ));
    }
    let index = field(&form, "index").parse::<usize>().unwrap_or(0);
    let view = if ctx.query.get("view").is_some_and(|view| view == "split") {
        "?view=split"
    } else {
        ""
    };
    Ok(ctx.redirect(&format!("{base}/diff{view}#diff-file-{index}")))
}

/// Discards the viewer's pending review and its comments, then returns to the Files changed page.
pub(crate) async fn native_discard_review(
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
    let base = format!("{}/{}", repo.view.family_url(Kind::Pull), number);
    match review_rules::discard_pending_review(&ctx.state.pool, user, owner, name, number)
        .await
        .map_err(operation_error)
    {
        Ok(()) => Ok(ctx.redirect(&format!("{base}/diff"))),
        Err(failure) => {
            let value = item(&ctx, &repo, Kind::Pull, number).await?;
            let (status, message) = safe_api_error(&failure);
            render_diff(&ctx, &repo, &value, None, None, Some(&message), status).await
        }
    }
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
                let view = if ctx.query.get("view").is_some_and(|view| view == "split") {
                    "?view=split"
                } else {
                    ""
                };
                format!("{base}/diff{view}#thread-{comment_id}")
            } else {
                let page = field(&form, "page").parse::<usize>().unwrap_or(1);
                let offset = field(&form, "review_comments")
                    .parse::<usize>()
                    .ok()
                    .filter(|offset| *offset <= MAX_REVIEW_COMMENT_OFFSET)
                    .unwrap_or(0);
                discussion_link_at(&base, page, offset, &format!("thread-{comment_id}"))
            };
            Ok(ctx.redirect(&location))
        }
        Err(failure) => {
            let value = item(&ctx, &repo, Kind::Pull, number).await?;
            let (status, message) = safe_api_error(&failure);
            if from_diff {
                render_diff(&ctx, &repo, &value, None, None, Some(&message), status).await
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

/// Changes the author's conversation comment from the Conversation tab. A preview shows the page
/// again with the text.
pub(crate) async fn native_edit_conversation_comment(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number, comment_id)): Path<(String, String, i32, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    let mut draft = submitted_detail(&form, "comment");
    draft.editing = Some(comment_id);
    let Ok(expected_body) = serde_json::from_str::<String>(&draft.expected_body) else {
        return render_detail(
            &ctx,
            &repo,
            Kind::Pull,
            &value,
            &draft,
            Some("The original comment is required. Reload before saving."),
            StatusCode::CONFLICT,
        )
        .await;
    };
    if draft.preview {
        return render_detail(
            &ctx,
            &repo,
            Kind::Pull,
            &value,
            &draft,
            None,
            StatusCode::OK,
        )
        .await;
    }
    let result = comment_rules::edit_pull_request_comment(
        &ctx.state.pool,
        &ctx.state.notification_broadcaster,
        ctx.require_user()?.id,
        crate::backend::issues::comments::CommentTarget {
            owner,
            repo: name,
            number,
            comment_id,
        },
        draft.comment.clone(),
        Some(&expected_body),
    )
    .await
    .map_err(operation_error);
    match result {
        Ok(comment) => {
            let base = format!("{}/{}", repo.view.family_url(Kind::Pull), number);
            let link = crate::backend::repositories::pull_requests::comments::saved_comment_link(
                &ctx.state.pool,
                &base,
                &comment,
            )
            .await;
            Ok(ctx.redirect(&link))
        }
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
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

/// Deletes a conversation comment from the Conversation tab, after the confirmation dialog.
pub(crate) async fn native_delete_conversation_comment(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number, comment_id)): Path<(String, String, i32, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    let result = comment_rules::delete_pull_request_comment(
        &ctx.state.pool,
        ctx.require_user()?.id,
        crate::backend::issues::comments::CommentTarget {
            owner,
            repo: name,
            number,
            comment_id,
        },
    )
    .await
    .map_err(operation_error);
    match result {
        Ok(()) => Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(Kind::Pull), number))),
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

use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::discussion_write_error;
use crate::backend::api::repos::chrome;
use crate::backend::api::repos::pulls::pull::*;
use crate::backend::db;
use crate::backend::db::reactions::{ReactionRefusal, Subject, CONTENTS};
use crate::backend::issues::MAX_COMMENT_PAGE;
use crate::webui::pages::repos::issues::{CommentPage, Item, Kind, Reaction};
use crate::webui::pages::repos::pulls::Thread;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::collections::HashMap;
use std::sync::Arc;

/// Gives or takes back a reaction on an issue or pull request, on one of its comments or on one of
/// its reviews. The `comment` or `review` field names the target. A reaction sent from the diff
/// (`from=diff`) returns to the diff, and one sent from a discussion page returns to that page.
pub(crate) async fn native_react(
    State(state): State<Arc<crate::backend::app::AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let kind = kind(&uri);
    let repo = repository(&ctx, &owner, &name).await?;
    let content = form.get("content").map(String::as_str).unwrap_or("");
    if !CONTENTS.contains(&content) {
        return Err(ctx.error(StatusCode::BAD_REQUEST, "Unknown reaction."));
    }
    let pool = &ctx.state.pool;
    let (item_subject, comment_subject, item_id) = match kind {
        Kind::Issue => (
            Subject::Issue,
            Subject::IssueComment,
            db::issues::get_issue(pool, repo.record.id, number)
                .await
                .map_err(|_| unavailable(&ctx))?
                .map(|issue| issue.id),
        ),
        Kind::Pull => (
            Subject::PullRequest,
            Subject::PullRequestComment,
            db::pull_requests::records::get_pull_request(pool, repo.record.id, number)
                .await
                .map_err(|_| unavailable(&ctx))?
                .map(|pull| pull.id),
        ),
    };
    let item_id = item_id.ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Not found."))?;
    let comment_id = form_id(&form, "comment")
        .map_err(|_| ctx.error(StatusCode::BAD_REQUEST, "Invalid comment."))?;
    let review_id = form_id(&form, "review")
        .map_err(|_| ctx.error(StatusCode::BAD_REQUEST, "Invalid review."))?;
    let (subject, subject_id) = match (comment_id, review_id) {
        (None, None) => (item_subject, item_id),
        (Some(id), None) => (comment_subject, id),
        (None, Some(id)) if kind == Kind::Pull => (Subject::PullRequestReview, id),
        _ => return Err(ctx.error(StatusCode::BAD_REQUEST, "Invalid reaction target.")),
    };
    // The write checks the reactor's access, the archive state and the target in its transaction.
    db::reactions::toggle_reaction(
        pool,
        ctx.require_user()?.id,
        repo.record.id,
        item_id,
        subject,
        subject_id,
        content,
    )
    .await
    .map_err(|error| reaction_error(&ctx, error))?;
    let base = format!("{}/{}", repo.view.family_url(kind), number);
    let page = if ctx.query.get("from").is_some_and(|from| from == "diff") {
        let split = if ctx.query.get("view").is_some_and(|view| view == "split") {
            "?view=split"
        } else {
            ""
        };
        format!("{base}/diff{split}")
    } else {
        // The discussion page the form was on, so the reaction comes back to it.
        match chrome::number(&ctx, "page", 1, MAX_COMMENT_PAGE)? {
            1 => base,
            page => format!("{base}?page={page}"),
        }
    };
    Ok(ctx.redirect(&match (comment_id, review_id) {
        (Some(id), _) => format!("{page}#comment-{id}"),
        (None, Some(id)) => format!("{page}#review-{id}"),
        (None, None) => page,
    }))
}

/// The page for a reaction the write refused. A target that is not on the item is a 404. Other
/// refusals use the same safe text as the JSON API.
fn reaction_error(ctx: &PageContext, error: anyhow::Error) -> Response {
    if error.downcast_ref::<ReactionRefusal>().is_some() {
        return ctx.error(StatusCode::NOT_FOUND, "Not found.");
    }
    let (status, message) = safe_api_error(&discussion_write_error(error));
    ctx.error(status, &message)
}

/// The id in a form field that is either empty or a number.
fn form_id(
    form: &HashMap<String, String>,
    field: &str,
) -> Result<Option<i64>, std::num::ParseIntError> {
    form.get(field)
        .filter(|value| !value.is_empty())
        .map(|value| value.parse::<i64>())
        .transpose()
}

/// The reactions on each of `ids`, grouped by the id they belong to.
async fn reactions_on(
    ctx: &PageContext,
    subject: Subject,
    ids: &[i64],
) -> ResultPage<HashMap<i64, Vec<Reaction>>> {
    let viewer = ctx.user.as_ref().map_or(0, |user| user.id);
    let rows = db::reactions::reaction_counts(&ctx.state.pool, subject.name(), ids, viewer)
        .await
        .map_err(|_| unavailable(ctx))?;
    let mut grouped: HashMap<i64, Vec<Reaction>> = HashMap::new();
    for row in rows {
        grouped.entry(row.subject_id).or_default().push(Reaction {
            content: row.content,
            count: row.count,
            mine: row.mine,
        });
    }
    Ok(grouped)
}

/// Fills in the reactions on the item, on the comments and reviews of one page of its discussion,
/// and on the inline comments inside those reviews.
pub(crate) async fn attach(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    item: &mut Item,
    comments: &mut CommentPage,
) -> ResultPage<()> {
    let pool = &ctx.state.pool;
    let (item_subject, comment_subject, item_id) = match kind {
        Kind::Issue => (
            Subject::Issue,
            Subject::IssueComment,
            db::issues::get_issue(pool, repo.record.id, item.number)
                .await
                .map_err(|_| unavailable(ctx))?
                .map(|issue| issue.id),
        ),
        Kind::Pull => (
            Subject::PullRequest,
            Subject::PullRequestComment,
            db::pull_requests::records::get_pull_request(pool, repo.record.id, item.number)
                .await
                .map_err(|_| unavailable(ctx))?
                .map(|pull| pull.id),
        ),
    };
    let Some(item_id) = item_id else {
        return Ok(());
    };
    item.reactions = reactions_on(ctx, item_subject, &[item_id])
        .await?
        .remove(&item_id)
        .unwrap_or_default();
    let ids: Vec<i64> = comments
        .comments
        .iter()
        .map(|comment| comment.id)
        .chain(
            comments
                .reviews
                .iter()
                .flat_map(|review| review.comments.iter().map(|comment| comment.id)),
        )
        .collect();
    let mut by_comment = reactions_on(ctx, comment_subject, &ids).await?;
    for comment in comments.comments.iter_mut().chain(
        comments
            .reviews
            .iter_mut()
            .flat_map(|review| review.comments.iter_mut()),
    ) {
        comment.reactions = by_comment.remove(&comment.id).unwrap_or_default();
    }
    let review_ids: Vec<i64> = comments.reviews.iter().map(|review| review.id).collect();
    let mut by_review = reactions_on(ctx, Subject::PullRequestReview, &review_ids).await?;
    for review in &mut comments.reviews {
        review.reactions = by_review.remove(&review.id).unwrap_or_default();
    }
    Ok(())
}

/// Fills in the reactions on the comments of the diff's inline threads. A pending thread gets none,
/// because nobody can react to it until its review is submitted.
pub(crate) async fn attach_threads(ctx: &PageContext, threads: &mut [Thread]) -> ResultPage<()> {
    let ids: Vec<i64> = threads
        .iter()
        .filter(|thread| !thread.pending)
        .flat_map(|thread| {
            std::iter::once(thread.root.id).chain(thread.replies.iter().map(|reply| reply.id))
        })
        .collect();
    let mut by_comment = reactions_on(ctx, Subject::PullRequestComment, &ids).await?;
    for thread in threads.iter_mut().filter(|thread| !thread.pending) {
        thread.root.reactions = by_comment.remove(&thread.root.id).unwrap_or_default();
        for reply in &mut thread.replies {
            reply.reactions = by_comment.remove(&reply.id).unwrap_or_default();
        }
    }
    Ok(())
}

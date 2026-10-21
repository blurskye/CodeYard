//! Ticking a task box in an issue or pull request body, or in a comment. The box flips in the
//! stored text and the text is saved through the same update the edit forms use. A writer may tick
//! a box in the body, the author of a comment may tick one in it, and a stale page is refused.

use super::issue::{save_entry_change, EntryChange};
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::discussion::*;
use crate::backend::api::repos::forms::{authorize_post, field};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::issues::comment_link;
use crate::backend::issues::comments::{CommentTarget, CreateCommentRequest};
use crate::backend::repositories::pull_requests::comments as comment_rules;
use crate::webui::components::task_lists::toggle_task;
use crate::webui::pages::repos::issues::{DetailDraft, Kind};
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::collections::HashMap;
use std::sync::Arc;

/// `task` is the box's number in the text; `comment` is the comment it is in, or empty for the
/// body. A body save also sends the `expected_state` and `expected_updated` it was read at.
pub(crate) async fn native_toggle_task(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let kind = kind(&uri);
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, kind, number).await?;
    let actor_id = ctx.require_user()?.id;
    let index = field(&form, "task")
        .parse::<usize>()
        .map_err(|_| ctx.error(StatusCode::BAD_REQUEST, "Choose a task to tick."))?;
    let base = format!("{}/{}", repo.view.family_url(kind), number);
    let comment_id = field(&form, "comment").parse::<i64>().ok();
    let text = match comment_id {
        None => value.body.clone(),
        Some(id) => {
            comment_body(
                &ctx,
                kind,
                (repo.record.id, number),
                id,
                actor_id,
                &field(&form, "comment_revision"),
            )
            .await?
        }
    };
    let Some(updated) = toggle_task(&text, index) else {
        return Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "That task is no longer in the text.",
        ));
    };
    let result = match comment_id {
        Some(id) => {
            save_comment_text(
                &ctx,
                kind,
                actor_id,
                (owner, name, number),
                id,
                (updated, text.as_str()),
                &base,
            )
            .await
        }
        None => save_entry_change(
            &ctx,
            kind,
            actor_id,
            (owner, name, number),
            EntryChange {
                title: Some(value.title.clone()),
                body: Some(updated),
                state: None,
                expected_state: field(&form, "expected_state"),
                expected_updated: field(&form, "expected_updated"),
            },
        )
        .await
        .map(|()| base.clone()),
    };
    match result {
        Ok(location) => Ok(ctx.redirect(&location)),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            render_detail(
                &ctx,
                &repo,
                kind,
                &value,
                &DetailDraft::default(),
                Some(&message),
                status,
            )
            .await
        }
    }
}

/// The text of comment `id`, read from the store that holds comments of `kind`.
async fn comment_body(
    ctx: &PageContext,
    kind: Kind,
    (repo_id, number): (i64, i32),
    id: i64,
    actor_id: i64,
    expected: &str,
) -> ResultPage<String> {
    let body = match kind {
        Kind::Issue => {
            let issue = db::issues::get_issue(&ctx.state.pool, repo_id, number)
                .await
                .map_err(|_| unavailable(ctx))?
                .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue not found."))?;
            db::issues::get_issue_comment(&ctx.state.pool, id)
                .await
                .map(|comment| {
                    comment
                        .filter(|comment| {
                            comment.issue_id == issue.id && comment.author_id == actor_id
                        })
                        .map(|comment| comment.body)
                })
        }
        // Only the author can edit a comment, so another account's comment, pending ones
        // included, is not found here.
        Kind::Pull => {
            let pull =
                db::pull_requests::records::get_pull_request(&ctx.state.pool, repo_id, number)
                    .await
                    .map_err(|_| unavailable(ctx))?
                    .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found."))?;
            db::pull_requests::reviews::get_pull_request_comment_by_id(&ctx.state.pool, id)
                .await
                .map(|comment| {
                    comment
                        .filter(|comment| {
                            comment.author_id == actor_id
                                && comment.pull_request_id == pull.id
                                && comment.review_id.is_none()
                        })
                        .map(|comment| comment.body)
                })
        }
    }
    .map_err(|_| unavailable(ctx))?;
    let body = body.ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Comment not found."))?;
    if expected != comment_revision(&body) {
        return Err(ctx.error(
            StatusCode::CONFLICT,
            "The comment changed. Reload before ticking a task.",
        ));
    }
    Ok(body)
}

/// Saves the ticked text of comment `id` through the rules for its kind, and returns the link to
/// the comment.
async fn save_comment_text(
    ctx: &PageContext,
    kind: Kind,
    actor_id: i64,
    (owner, name, number): (String, String, i32),
    id: i64,
    (body, expected_body): (String, &str),
    base: &str,
) -> Result<String, ApiFailure> {
    let pool = &ctx.state.pool;
    let broadcaster = &ctx.state.notification_broadcaster;
    let target = CommentTarget {
        owner,
        repo: name,
        number,
        comment_id: id,
    };
    match kind {
        Kind::Issue => {
            crate::backend::issues::comments::edit_issue_comment(
                pool,
                broadcaster,
                actor_id,
                target,
                CreateCommentRequest {
                    body,
                    ..Default::default()
                },
                Some(expected_body),
            )
            .await
            .map_err(crate::backend::api::repos::issues::operation_error)?;
            let position = db::issues::issue_comment_position(pool, id).await;
            Ok(comment_link(base, id, position))
        }
        Kind::Pull => {
            let comment = comment_rules::edit_pull_request_comment(
                pool,
                broadcaster,
                actor_id,
                target,
                body,
                Some(expected_body),
            )
            .await
            .map_err(crate::backend::api::repos::pulls::operation_error)?;
            let position = db::pull_requests::reviews::count_discussion_items_through(
                pool,
                comment.pull_request_id,
                db::pull_requests::reviews::DiscussionRef::of_comment(&comment),
            )
            .await;
            Ok(comment_link(base, id, position))
        }
    }
}

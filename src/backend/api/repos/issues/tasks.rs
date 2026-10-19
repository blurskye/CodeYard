//! Ticking a task box in an issue or pull request body, or in a comment. The box flips in the
//! stored text and the text is saved through the same update the edit forms use, so the author or
//! a writer may tick, and a stale page is refused.

use super::issue::{save_entry_text, EntryText};
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::pulls::pull::*;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::issues::comment_link;
use crate::backend::issues::comments::{CommentTarget, CreateCommentRequest};
use crate::webui::components::task_lists::toggle_task;
use crate::webui::pages::repos::issues::DetailDraft;
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
            db::issues::get_issue_comment(&ctx.state.pool, id)
                .await
                .map_err(|_| unavailable(&ctx))?
                .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Comment not found."))?
                .body
        }
    };
    let Some(updated) = toggle_task(&text, index) else {
        return Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "That task is no longer in the text.",
        ));
    };
    let result = match comment_id {
        Some(id) => crate::backend::issues::comments::edit_issue_comment(
            &ctx.state.pool,
            &ctx.state.notification_broadcaster,
            actor_id,
            CommentTarget {
                owner,
                repo: name,
                number,
                comment_id: id,
            },
            CreateCommentRequest { body: updated },
        )
        .await
        .map_err(crate::backend::api::repos::issues::operation_error)
        .map(|_| ()),
        None => {
            save_entry_text(
                &ctx,
                kind,
                actor_id,
                (owner, name, number),
                EntryText {
                    title: value.title.clone(),
                    body: updated,
                    expected_state: field(&form, "expected_state"),
                    expected_updated: field(&form, "expected_updated"),
                },
            )
            .await
        }
    };
    match result {
        Ok(()) => match comment_id {
            Some(id) => {
                let position = db::issues::issue_comment_position(&ctx.state.pool, id).await;
                Ok(ctx.redirect(&comment_link(&base, id, position)))
            }
            None => Ok(ctx.redirect(&base)),
        },
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

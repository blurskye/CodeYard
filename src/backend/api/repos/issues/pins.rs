use crate::backend::{
    api::{
        http::context::PageContext,
        repos::{
            discussion::{kind, repository, unavailable, RepositoryData, ResultPage},
            forms::{authorize_post, field},
        },
    },
    app::AppState,
    db,
};
use crate::webui::pages::repos::issues::{self as view, Kind};
use axum::{
    extract::{Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use std::{collections::HashMap, sync::Arc};

pub(crate) async fn native_pin(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number, comment)): Path<(String, String, i32, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let kind = kind(&uri);
    let item = super::assignees::assigned_item(&ctx, &repo, kind, number).await?;
    let pinned = match field(&form, "pinned").as_str() {
        "true" => true,
        "false" => false,
        _ => return Err(ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid pin state.")),
    };
    db::pins::set_pin(
        &ctx.state.pool,
        ctx.require_user()?.id,
        &db::users::accounts::AccountCredential::Session(
            &crate::backend::api::http::session::extract_session_id(&ctx.headers)
                .unwrap_or_default(),
        ),
        repo.record.id,
        item,
        comment,
        pinned,
    )
    .await
    .map_err(|error| match error.downcast_ref::<db::pins::PinRefusal>() {
        Some(db::pins::PinRefusal::Denied) => ctx.error(
            StatusCode::FORBIDDEN,
            "Pinning comments requires write access.",
        ),
        Some(db::pins::PinRefusal::Missing) => {
            ctx.error(StatusCode::NOT_FOUND, "Comment not found.")
        }
        Some(db::pins::PinRefusal::Occupied) => ctx.error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Unpin the current comment before pinning another.",
        ),
        None => unavailable(&ctx),
    })?;
    Ok(ctx.redirect(&format!("{}/{number}", repo.view.family_url(kind))))
}

pub(crate) async fn pinned_comment(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    number: i32,
    item: db::assignees::AssignedItem,
) -> ResultPage<Option<Box<view::PinnedComment>>> {
    let Some(pin) = db::pins::get_pin(&ctx.state.pool, item)
        .await
        .map_err(|_| unavailable(ctx))?
    else {
        return Ok(None);
    };
    let base = format!("{}/{number}", repo.view.family_url(kind));
    let (body, author, href) = match item {
        db::assignees::AssignedItem::Issue(id) => {
            // A comment deleted after the pin was read takes its pin with it.
            let Some(comment) = db::issues::get_issue_comment(&ctx.state.pool, pin.comment_id)
                .await
                .map_err(|_| unavailable(ctx))?
                .filter(|comment| comment.issue_id == id)
            else {
                return Ok(None);
            };
            let position = db::issues::issue_comment_position(&ctx.state.pool, comment.id).await;
            let href = crate::backend::issues::comment_link(&base, comment.id, position);
            (comment.body, comment.author_id, href)
        }
        db::assignees::AssignedItem::PullRequest(id) => {
            let Some(comment) = db::pull_requests::reviews::get_pull_request_comment_by_id(
                &ctx.state.pool,
                pin.comment_id,
            )
            .await
            .map_err(|_| unavailable(ctx))?
            .filter(|comment| {
                comment.pull_request_id == id
                    && comment.path.is_none()
                    && comment.review_id.is_none()
            }) else {
                return Ok(None);
            };
            let href = crate::backend::repositories::pull_requests::comments::saved_comment_link(
                &ctx.state.pool,
                &base,
                &comment,
            )
            .await;
            (comment.body, comment.author_id, href)
        }
    };
    let author = db::users::accounts::get_user_by_id(&ctx.state.pool, author)
        .await
        .map_err(|_| unavailable(ctx))?
        .map_or_else(|| "Deleted user".into(), |user| user.username);
    let pinned_by = match pin.pinned_by {
        Some(id) => db::users::accounts::get_user_by_id(&ctx.state.pool, id)
            .await
            .map_err(|_| unavailable(ctx))?
            .map(|user| user.username),
        None => None,
    };
    Ok(Some(Box::new(view::PinnedComment {
        id: pin.comment_id,
        author,
        body,
        href,
        pinned_by,
        pinned_at: pin.pinned_at.to_rfc3339(),
    })))
}

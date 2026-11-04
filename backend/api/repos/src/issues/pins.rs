use crate::discussion::kind;
use crate::discussion::repository;
use crate::discussion::unavailable;
use crate::discussion::RepositoryData;
use crate::discussion::ResultPage;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::repos::forms::authorize_post;
use codeyard_api_http::shared::repos::forms::field;
use codeyard_app::app::AppState;
use codeyard_webui::pages::repos::issues as view;
use codeyard_webui::pages::repos::issues::Kind;
use std::collections::HashMap;
use std::sync::Arc;

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
    let item = crate::issues::assignees::assigned_item(&ctx, &repo, kind, number).await?;
    let pinned = match field(&form, "pinned").as_str() {
        "true" => true,
        "false" => false,
        _ => return Err(ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid pin state.")),
    };
    codeyard_db_repos::pins::set_pin(
        &ctx.state.pool,
        ctx.require_user()?.id,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default(),
        ),
        repo.record.id,
        item,
        comment,
        pinned,
    )
    .await
    .map_err(
        |error| match error.downcast_ref::<codeyard_db_repos::pins::PinRefusal>() {
            Some(codeyard_db_repos::pins::PinRefusal::Denied) => ctx.error(
                StatusCode::FORBIDDEN,
                "Pinning comments requires write access.",
            ),
            Some(codeyard_db_repos::pins::PinRefusal::Missing) => {
                ctx.error(StatusCode::NOT_FOUND, "Comment not found.")
            }
            Some(codeyard_db_repos::pins::PinRefusal::Occupied) => ctx.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Unpin the current comment before pinning another.",
            ),
            None => unavailable(&ctx),
        },
    )?;
    Ok(ctx.redirect(&format!("{}/{number}", repo.view.family_url(kind))))
}

pub(crate) async fn pinned_comment(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    number: i32,
    item: codeyard_db_repos::assignees::AssignedItem,
) -> ResultPage<Option<Box<view::PinnedComment>>> {
    let Some(pin) = codeyard_db_repos::pins::get_pin(&ctx.state.pool, item)
        .await
        .map_err(|_| unavailable(ctx))?
    else {
        return Ok(None);
    };
    let base = format!("{}/{number}", repo.view.family_url(kind));
    let (body, author, href) = match item {
        codeyard_db_repos::assignees::AssignedItem::Issue(id) => {
            // A comment deleted after the pin was read takes its pin with it.
            let Some(comment) =
                codeyard_db_repos::issues::get_issue_comment(&ctx.state.pool, pin.comment_id)
                    .await
                    .map_err(|_| unavailable(ctx))?
                    .filter(|comment| comment.issue_id == id)
            else {
                return Ok(None);
            };
            let position =
                codeyard_db_repos::issues::issue_comment_position(&ctx.state.pool, comment.id)
                    .await;
            let href = codeyard_repos_pulls::issues::comment_link(&base, comment.id, position);
            (comment.body, comment.author_id, href)
        }
        codeyard_db_repos::assignees::AssignedItem::PullRequest(id) => {
            let Some(comment) =
                codeyard_db_repos::pull_requests::reviews::get_pull_request_comment_by_id(
                    &ctx.state.pool,
                    pin.comment_id,
                )
                .await
                .map_err(|_| unavailable(ctx))?
                .filter(|comment| {
                    comment.pull_request_id == id
                        && comment.path.is_none()
                        && comment.review_id.is_none()
                })
            else {
                return Ok(None);
            };
            let href =
                codeyard_repos_pulls::repositories::pull_requests::comments::saved_comment_link(
                    &ctx.state.pool,
                    &base,
                    &comment,
                )
                .await;
            (comment.body, comment.author_id, href)
        }
    };
    let author = codeyard_db_identity::users::accounts::get_user_by_id(&ctx.state.pool, author)
        .await
        .map_err(|_| unavailable(ctx))?
        .map_or_else(|| "Deleted user".into(), |user| user.username);
    let pinned_by = match pin.pinned_by {
        Some(id) => codeyard_db_identity::users::accounts::get_user_by_id(&ctx.state.pool, id)
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

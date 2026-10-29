use crate::backend::{
    api::{
        http::context::PageContext,
        repos::{
            discussion::{kind, repository, unavailable, ResultPage},
            forms::{authorize_post, field},
        },
    },
    app::AppState,
    db,
};
use axum::{
    extract::{Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use std::{collections::HashMap, sync::Arc};

pub(crate) async fn native_lock(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let kind = kind(&uri);
    let item = super::assignees::assigned_item(&ctx, &repo, kind, number).await?;
    let locked = match field(&form, "locked").as_str() {
        "true" => true,
        "false" => false,
        _ => return Err(ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid lock state.")),
    };
    let reason = field(&form, "lock_reason");
    db::discussions::set_lock(
        &ctx.state.pool,
        ctx.require_user()?.id,
        repo.record.id,
        item,
        locked,
        (!reason.is_empty()).then_some(reason.as_str()),
    )
    .await
    .map_err(
        |error| match error.downcast_ref::<db::discussions::LockRefusal>() {
            Some(db::discussions::LockRefusal::Denied) => ctx.error(
                StatusCode::FORBIDDEN,
                "Write access is required to lock this conversation.",
            ),
            Some(db::discussions::LockRefusal::Missing) => {
                ctx.error(StatusCode::NOT_FOUND, "Discussion not found.")
            }
            Some(db::discussions::LockRefusal::InvalidReason) => {
                ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid lock reason.")
            }
            None => unavailable(&ctx),
        },
    )?;
    Ok(ctx.redirect(&format!("{}/{number}", repo.view.family_url(kind))))
}
